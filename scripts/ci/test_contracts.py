"""Contracts between CI and the release, publication and signing machinery.

Each test imports the code on the other side of its contract (the publishers,
the release preflight) instead of copying it. A failure means a release, update
feed or signed build would break: fix the workflow, not the contract.
"""

from __future__ import annotations

import importlib.util
import itertools
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

import cargo_graph_key
import plan
import windows_signature_reference as signature_probe
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
    "Linux-sccache-", "macOS-sccache-", "${{ runner.os }}-sccache-", "xcode-cache-",
    "maple-dev-xcode", "release-onnxruntime-ios-", "${{ runner.os }}-bun-", "${{ runner.os }}-gradle-",
)
ONNX_CACHE = "apps/maple-research/frontend/src-tauri/onnxruntime-ios"
# The unsigned Agent builders that write the exact-key release cache; no ci.yml lane reads it.
# Its key hashes the Agent's dependency graph and toolchain with cargo_graph_key.py.
AGENT_RELEASE_WRITERS = frozenset(("agent-desktop-build.yml", job) for job in ("macos-build", "windows-build", "linux"))
AGENT_RELEASE_READERS: frozenset = frozenset()
# The packaging rehearsals build thin LTO into their own cache, saved only by
# ci.yml's nightly master run.
AGENT_PACKAGE_JOBS = frozenset(("ci.yml", f"agent-package-{os}") for os in ("linux", "macos", "windows"))
# Jobs that may mint an OIDC token: attestation, signing identity, FlakeHub, registries.
OIDC_JOBS = frozenset({
    ("agent-desktop-build.yml", "macos"), ("agent-desktop-build.yml", "windows"),
    ("agent-desktop-build.yml", "attest-linux"), ("android-build.yml", "build-android"),
    ("desktop-build.yml", "build-macos"), ("desktop-build.yml", "build-linux"), ("desktop-build.yml", "build-windows"),
    ("dev-credential-requester-image.yml", "build"), ("ios-dev-testflight.yml", "build-ios-dev"),
    ("mobile-build.yml", "build-ios"), ("opensecret-eif-release.yml", "build"),
    ("ci.yml", "os-eif"), ("opensecret-eif.yml", "eif-trusted"),
    *(("release.yml", job) for job in ("build-tauri", "build-windows", "build-android", "build-ios", "build-web",
                                       "publish-proxy-release-artifacts", "update-latest-json",
                                       "publish-release-verification-guide")),
    ("sdk-publish-npm.yml", "publish"), ("sdk-publish-rust.yml", "publish"), ("web-build.yml", "build-web-master"),
})
# The EIF builders that warm FlakeHub for opensecret-eif-release.yml.
FLAKEHUB_JOBS = frozenset({("ci.yml", "os-eif"), ("opensecret-eif.yml", "eif-trusted"),
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

def condition(job: dict) -> str:
    return " ".join(str(job.get("if", "")).split())


def variants(job: dict, event: str) -> list:
    """The job's `variant` matrix on `event`: a list, or `fromJSON(<condition> && '[...]' || '[...]')`."""
    matrix = job.get("strategy", {}).get("matrix", {}).get("variant")
    if matrix is None or isinstance(matrix, list):
        return matrix or [None]
    text = matrix.strip().removeprefix("${{").removesuffix("}}").strip().removeprefix("fromJSON(").removesuffix(")")
    return json.loads(expression_value(text.replace("&&", " and ").replace("||", " or "),
                                       {"github.event_name": event}))


def job_results(config: dict, event: str) -> dict:
    """Which agent-desktop-build.yml jobs run on a master `event` that changes Agent
    inputs, if every job that runs succeeds: each job's `if:` and `needs`, evaluated."""
    results: dict = {}
    pending = dict(config["jobs"])
    while pending:
        ready = [job_id for job_id, job in pending.items()
                 if set(needs_of(job)) <= set(results)]
        if not ready:
            raise ValueError(f"unresolvable needs: {sorted(pending)}")
        for job_id in ready:
            job = pending.pop(job_id)
            succeeded = all(results[need] == "success" for need in needs_of(job))
            if "if" not in job:
                runs = succeeded
            else:
                text = expression(job)
                if not re.search(r"\b(?:always|cancelled|failure|success)\(\)", str(job["if"])):
                    text = f"{succeeded} and ({text})"  # GitHub's implicit success()
                context = {"NOT_CANCELLED": True, "github.repository": publisher.REPOSITORY,
                           "github.ref": "refs/heads/master", "github.event_name": event,
                           "needs.changes.outputs.agent": "true" if results.get("changes") == "success" else ""}
                context.update({f"needs.{need}.result": results[need] for need in needs_of(job)})
                runs = evaluate(text, dict(sorted(context.items(), key=lambda item: -len(item[0]))))
            results[job_id] = "success" if runs else "skipped"
    return results


def needs_of(job: dict) -> list:
    value = job.get("needs", [])
    return [value] if isinstance(value, str) else list(value)


def desktop_run(config: dict, run_id: int, event: str = "push"):
    """The job inventory (with runner labels) and artifacts of one agent-desktop-build.yml
    run on master in which every job that runs succeeds; a job its `if:` skips is skipped."""
    def expand(text, variant):
        return text.replace("${{ matrix.variant }}", str(variant)).replace("${{ github.run_id }}", str(run_id))

    results = job_results(config, event)
    inventory, artifacts = [], []
    for job_id, job in config["jobs"].items():
        for variant in variants(job, event):
            inventory.append({"name": expand(job.get("name", job_id), variant), "status": "completed",
                              "conclusion": results[job_id], "labels": [job["runs-on"]]})
            if results[job_id] == "success":
                artifacts += [{"name": expand(step["with"]["name"], variant), "expired": False}
                              for step in job.get("steps", [])
                              if step.get("uses", "").startswith("actions/upload-artifact@")]
    return inventory, artifacts


def cache_steps():
    for name, job_id, job in jobs():
        for step in job.get("steps", []):
            if step.get("uses", "").startswith(("actions/cache", "irgaly/xcode-cache@")):
                yield name, job_id, step


def expression(job: dict) -> str:
    """A job's `if:` as a Python expression with equivalent semantics for these gates."""
    text = job["if"].strip().removeprefix("${{").removesuffix("}}").replace("&&", " and ").replace("||", " or ")
    return " ".join(text.replace("!cancelled()", "NOT_CANCELLED").replace("always()", "True").split())


def expression_value(text: str, context: dict):
    for key, replacement in context.items():
        text = text.replace(key, repr(replacement))
    literals = {"true": True, "false": False, "null": None}  # GitHub's literals; quoted strings are untouched
    return eval(text, {"__builtins__": {}}, literals)  # noqa: S307 - fixed workflow text


def evaluate(text: str, context: dict) -> bool:
    return bool(expression_value(text, context))


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
        self.assertTrue(config["on"].get("schedule"))  # Dev packages build nightly
        run_id = 4242
        for job_id, job in config["jobs"].items():
            attests = any(step.get("uses", "").startswith("actions/attest@") for step in job.get("steps", []))
            if attests:  # the publisher verifies with --deny-self-hosted-runners
                self.assertTrue(GITHUB_HOSTED.fullmatch(job["runs-on"]), job_id)
            if attests or environment(job) or job.get("name", "").startswith("Verify"):
                self.assertFalse([step for step in job.get("steps", []) if step.get("uses", "").startswith(
                    ("actions/cache", "Swatinem/rust-cache@"))], f"{job_id} must restore no cache")
        # Each job's `if:` and `needs` decide whether it runs: a Prod job they skip fails the publisher.
        # A push builds only Prod, the nightly run only Dev (never published), a dispatch both.
        profiles = {"push": ["prod"], "workflow_dispatch": ["dev", "prod"], "schedule": ["dev"]}
        for event, expected in profiles.items():
            results = job_results(config, event)
            for job_id, job in config["jobs"].items():
                if "variant" in job.get("strategy", {}).get("matrix", {}):
                    self.assertEqual((results[job_id], variants(job, event)), ("success", expected), (event, job_id))
            inventory, artifacts = desktop_run(config, run_id, event)
            run = {"id": run_id, "run_attempt": 1, "run_number": 77, "path": publisher.WORKFLOW,
                   "head_branch": "master", "event": event, "status": "completed", "head_sha": "a" * 40,
                   "created_at": "2026-01-01T00:00:00Z", "repository": {"full_name": publisher.REPOSITORY},
                   "head_repository": {"full_name": publisher.REPOSITORY}}
            responses = {"": run, "jobs": {"total_count": len(inventory), "jobs": inventory},
                         "artifacts": {"total_count": len(artifacts), "artifacts": artifacts}}
            with self.subTest(event=event), mock.patch.object(
                    publisher, "gh_json", lambda path: responses[re.sub(r".*/runs/\d+/?([a-z]*).*", r"\1", path)]):
                if "prod" not in expected:
                    with self.assertRaises(publisher.PublishError):
                        publisher.trust_run(run_id, linux=True, windows=True)
                    # Even a nightly run with every Prod job and artifact is never published.
                    inventory, artifacts = desktop_run(config, run_id, "push")
                    responses.update(jobs={"total_count": len(inventory), "jobs": inventory},
                                     artifacts={"total_count": len(artifacts), "artifacts": artifacts})
                    with self.assertRaisesRegex(publisher.PublishError, "untrusted build workflow or ref"):
                        publisher.trust_run(run_id, linux=True, windows=True)
                    continue
                proof = publisher.trust_run(run_id, linux=True, windows=True)
                self.assertEqual(proof["build"], "77")  # run_number is the Agent build number
        # The publisher checks the Windows package's build against that run number.
        windows = [job for job in config["jobs"].values()
                   if re.search(r"windows-release\.py (?:package|verify)", json.dumps(job.get("steps", [])))]
        self.assertTrue(windows)
        for job in windows:
            self.assertEqual(job.get("env", {}).get("MAPLE_PACKAGE_BUILD_NUMBER"), "${{ github.run_number }}",
                             job.get("name"))

    def test_c2_prebuilt_binary_handoff_preserves_the_packagers_release_path(self):
        config = load(ROOT / publisher.WORKFLOW)
        for platform, binary in (("macos", "maple-agent"), ("windows", "maple-agent.exe")):
            with self.subTest(platform=platform):
                producer = config["jobs"][platform + "-build"]["steps"]
                consumer = config["jobs"][platform]["steps"]
                upload = next(step["with"] for step in producer
                              if step.get("uses", "").startswith("actions/upload-artifact@")
                              and "-prebuilt-" in step["with"]["name"])
                download = next(step["with"] for step in consumer
                                if step.get("uses", "").startswith("actions/download-artifact@"))
                # A single uploaded file is stored at the artifact root. The
                # consumer must place that basename back under target/release.
                expected = Path("apps/maple-agent/target/release") / binary
                self.assertEqual(upload["path"].splitlines(), [expected.as_posix()])
                self.assertEqual(download["name"], upload["name"])
                self.assertEqual(Path(download["path"]) / binary, expected)

    def test_c2_windows_packaging_lanes_pass_a_build_number(self):
        # windows-release.py package and verify refuse to run without MAPLE_PACKAGE_BUILD_NUMBER.
        # A lane passes the Actions run number, or 1 outside Actions. Run each such lane's
        # command with stand-ins for the build script and Python, and read what they received.
        packaging = [lane for lane in plan.LANES if re.search(r"windows-release\.py (?:package|verify)", lane.run)]
        self.assertTrue(packaging)
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            stubs = {root / "scripts/build-release.sh": "exit 0\n",
                     root / "bin/python3": 'printf "%s|%s\\n" "${MAPLE_PACKAGE_BUILD_NUMBER-unset}" "$*" >> "$CALLS"\n'}
            for path, body in stubs.items():
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text("#!/bin/sh\n" + body)
                path.chmod(0o755)
            calls = root / "calls"
            for lane, run_number in itertools.product(packaging, ("77", None)):
                calls.write_text("")
                env = {key: value for key, value in os.environ.items()
                       if key not in ("GITHUB_RUN_NUMBER", "MAPLE_PACKAGE_BUILD_NUMBER")}
                env.update(dict(lane.env), PATH=f"{root / 'bin'}{os.pathsep}{env['PATH']}", CALLS=str(calls))
                if run_number:
                    env["GITHUB_RUN_NUMBER"] = run_number
                result = subprocess.run([shutil.which("bash"), "--noprofile", "--norc", "-euo", "pipefail", "-c",
                                         lane.run], cwd=root, env=env, capture_output=True, text=True)
                with self.subTest(lane=lane.id, run_number=run_number):
                    self.assertEqual(result.returncode, 0, result.stderr)
                    received = [line.split("|")[0] for line in calls.read_text().splitlines()
                                if re.search(r"windows-release\.py (?:package|verify) ", line)]
                    self.assertGreaterEqual(len(received), 2)  # package, then verify
                    self.assertEqual(set(received), {run_number or "1"})

    def test_c2_windows_signature_probe_accepts_this_workflows_prod_run(self):
        self.assertEqual(signature_probe.PRODUCER, publisher.WORKFLOW)
        self.assertEqual(signature_probe.REPOSITORY, publisher.REPOSITORY)
        run_id, source = 4242, "a" * 40
        inventory, artifacts = desktop_run(load(ROOT / publisher.WORKFLOW), run_id)
        signed = f"maple-agent-prod-windows-x86_64-{run_id}"
        self.assertIn(signed, [artifact["name"] for artifact in artifacts])
        jobs_ = [dict(job, id=index, started_at="2026-01-01T00:00:00Z", completed_at="2026-01-01T00:10:00Z")
                 for index, job in enumerate(inventory, 1)]
        run = {"id": run_id, "run_attempt": 1, "head_sha": source, "head_branch": "master", "event": "push",
               "path": publisher.WORKFLOW, "repository": {"full_name": publisher.REPOSITORY, "id": 1},
               "head_repository": {"full_name": publisher.REPOSITORY}, "status": "completed",
               "conclusion": "success"}
        artifact = {"id": 7, "expired": False, "name": signed, "size_in_bytes": 1000, "digest": "sha256:" + "b" * 64,
                    "created_at": "2026-01-01T00:05:00Z", "workflow_run": {
                        "id": run_id, "head_sha": source, "repository_id": 1, "head_repository_id": 1}}
        selected = signature_probe.selection({
            "MAPLE_PROBE_RUN": str(run_id), "MAPLE_PROBE_ATTEMPT": "1", "MAPLE_PROBE_ARTIFACT": "7",
            "MAPLE_PROBE_ZIP_BYTES": "1000", "MAPLE_PROBE_SOURCE": source, "MAPLE_PROBE_ZIP_SHA": "b" * 64,
            "MAPLE_PROBE_PE_MEMBER": "setup.exe", "MAPLE_PROBE_PE_SHA": "c" * 64})
        chosen = signature_probe.validate_producer(
            selected, run, artifact, {"total_count": len(jobs_), "jobs": jobs_})
        self.assertEqual([job["name"] for job in chosen], list(signature_probe.JOBS))

    def test_c3_release_preflight_finds_its_evidence_jobs_on_master_pushes(self):
        for name, required in ci_evidence.WORKFLOWS.items():
            config = workflows().get(name)
            with self.subTest(workflow=name):
                self.assertIsNotNone(config)
                push = config["on"].get("push") if isinstance(config["on"], dict) else None
                names = {job.get("name", job_id) for job_id, job in config["jobs"].items()}
                found = set(required) <= names if required else any(n.startswith("Analyze (") for n in names)
                self.assertTrue(found)
                self.assertIsInstance(push, dict)
                self.assertIn("master", push.get("branches", ["master"]))
                self.assertFalse(cancels(config.get("concurrency")))
                self.assertFalse(any(cancels(job.get("concurrency")) for job in config["jobs"].values()))

    def test_c4_signed_lanes_own_the_release_cache_keys(self):
        steps = list(cache_steps())
        writers = {(name, job_id) for name, job_id, step in steps if cache_writer(step)
                   and name not in RELEASE_FEEDING and step["with"].get("key", "").startswith(RESERVED_PREFIXES)}
        self.assertEqual(writers, set())
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
        self.assertLessEqual({"release.yml", "mobile-build.yml", "ios-dev-testflight.yml", "ci.yml"},
                             {n for n, _ in onnx})
        release = next(step["with"] for name, step in onnx if name == "release.yml")
        for name, step in onnx:
            with self.subTest(workflow=name):
                self.assertEqual((step["with"]["key"], step["with"]["restore-keys"]),
                                 (release["key"], release["restore-keys"]))
                if cache_writer(step):
                    self.assertIn(name, RELEASE_FEEDING)

    def test_c4_ci_restores_only_what_a_master_workflow_saves(self):
        # Same key and paths as a writer that only master pushes run, so PR lanes start warm.
        saved = {(step["with"]["key"], step["with"].get("path", "").strip()) for name, _, step in cache_steps()
                 if cache_writer(step) and triggers(workflows()[name]) <= {"push", "workflow_dispatch"}}
        for name, job_id, step in cache_steps():
            if (name, job_id) in AGENT_PACKAGE_JOBS:
                with self.subTest(job=job_id):
                    if step["uses"].startswith("actions/cache/save@"):
                        self.assertIn("refs/heads/master' && github.event_name == 'schedule'", step["if"])
                    else:
                        self.assertTrue(step["with"]["key"].startswith("ci-v1-agent-package-${{ runner.os }}-"))
                        self.assertEqual(step["with"]["restore-keys"], "ci-v1-agent-package-${{ runner.os }}-")
            elif name == "ci.yml":
                with self.subTest(job=job_id):
                    self.assertTrue(step["uses"].startswith("actions/cache/restore@"))
                    if not step["with"]["key"].startswith("ci-v1-agent-release-"):  # the next test
                        self.assertIn((step["with"]["key"], step["with"]["path"].strip()), saved)

    def test_c4_agent_release_cache_is_exact_and_written_only_by_master_builders(self):
        restores = [(name, job_id, step["with"]) for name, job_id, step in cache_steps()
                    if step["with"].get("key", "").startswith("ci-v1-agent-release-")]
        self.assertEqual({(name, job_id) for name, job_id, _ in restores},
                         AGENT_RELEASE_WRITERS | AGENT_RELEASE_READERS)
        self.assertEqual(len({(cache["key"], cache["path"]) for _, _, cache in restores}), 1)
        self.assertFalse([cache for _, _, cache in restores if "restore-keys" in cache])  # exact keys only
        # No other job falls back into the family by prefix.
        fallbacks = {(name, job_id) for name, job_id, step in cache_steps()
                     for line in str(step["with"].get("restore-keys", "")).splitlines()
                     if line.strip().startswith("ci-v1-agent-release-")}
        self.assertEqual(fallbacks, set())
        writers =[(name, job_id, step) for name, job_id, step in cache_steps() if cache_writer(step)
                   and (name, job_id) in AGENT_RELEASE_WRITERS | AGENT_RELEASE_READERS]
        self.assertEqual({(name, job_id) for name, job_id, _ in writers}, AGENT_RELEASE_WRITERS)
        for name, job_id, step in writers:
            with self.subTest(job=job_id):
                self.assertTrue(step["uses"].startswith("actions/cache/save@"))
                self.assertEqual(step["with"]["path"], restores[0][2]["path"])
                # Dev and Prod share the key. Only master pushes, which build only Prod, save it;
                # nightly and dispatched builds only restore it.
                saves = [event for event in ("push", "schedule", "workflow_dispatch") if evaluate(expression(step), {
                    "steps.cache.outputs.cache-hit": "false", "github.event_name": event,
                    "github.ref": "refs/heads/master"})]
                self.assertEqual(saves, ["push"])
                self.assertEqual(variants(workflows()[name]["jobs"][job_id], "push"), ["prod"])

    def test_c4_agent_release_key_hashes_the_dependency_graph_and_toolchain(self):
        # Each builder computes the key's hash in an unconditional step before the restore.
        # Run that step as the runner would: its output must be cargo_graph_key.py's hash of
        # the Agent's lockfiles, and a failing script must fail the step with no output. An
        # empty or misnamed output would turn the exact key into a prefix that restores any
        # entry of the family.
        agent = ROOT / "apps/maple-agent"
        expected = cargo_graph_key.key((agent / "Cargo.lock").read_text(), (agent / "flake.lock").read_text())
        pattern = r"ci-v1-agent-release-\$\{\{ runner\.os \}\}-\$\{\{ steps\.([\w-]+)\.outputs\.([\w-]+) \}\}"
        with tempfile.TemporaryDirectory() as temporary:
            working = Path(temporary) / "bin/python3"
            working.parent.mkdir()
            working.symlink_to(sys.executable)
            failing = Path(temporary) / "failing/python3"
            failing.parent.mkdir()
            failing.write_text(f"#!{sys.executable}\nraise SystemExit(1)\n")
            failing.chmod(0o755)

            def run(step, directory, python, output):
                return subprocess.run(
                    [shutil.which("bash"), "--noprofile", "--norc", "-eo", "pipefail", "-c", step["run"]],
                    cwd=ROOT / directory, capture_output=True, text=True,
                    env={**os.environ, "GITHUB_OUTPUT": str(output),
                         "PATH": f"{python.parent}{os.pathsep}{os.environ['PATH']}"})

            for name, job_id in sorted(AGENT_RELEASE_WRITERS):
                workflow = workflows()[name]
                steps = workflow["jobs"][job_id]["steps"]
                restore = next(index for index, step in enumerate(steps)
                               if step.get("uses", "").startswith("actions/cache/restore@")
                               and step["with"]["key"].startswith("ci-v1-agent-release-"))
                with self.subTest(job=job_id):
                    reference = re.fullmatch(pattern, steps[restore]["with"]["key"])
                    self.assertIsNotNone(reference)
                    producer = next(index for index, step in enumerate(steps) if step.get("id") == reference[1])
                    step = steps[producer]
                    self.assertLess(producer, restore)
                    self.assertFalse({"if", "continue-on-error", "shell"} & set(step))
                    defaults = {**workflow.get("defaults", {}).get("run", {}),
                                **workflow["jobs"][job_id].get("defaults", {}).get("run", {})}
                    self.assertEqual(defaults.get("shell"), "bash")
                    directory = step.get("working-directory", defaults.get("working-directory", "."))
                    output = Path(temporary) / f"{job_id}.output"
                    result = run(step, directory, working, output)
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual(output.read_text().splitlines(), [f"{reference[2]}={expected}"])
                    failed = Path(temporary) / f"{job_id}.failed"
                    self.assertNotEqual(run(step, directory, failing, failed).returncode, 0)
                    self.assertEqual(failed.read_text() if failed.exists() else "", "")

    def test_c4_lane_caches_fall_back_by_lockfile_only_within_their_own_family(self):
        # The planner's shared key holds only toolchain pins, so rust-cache must hash the
        # lockfiles itself: after the restore key, so a lockfile change restores by prefix.
        families = set()
        for job_id in plan.MATRIX_JOBS:
            steps = workflows()["ci.yml"]["jobs"][job_id]["steps"]
            index = next(i for i, step in enumerate(steps) if step.get("uses", "").startswith("Swatinem/rust-cache@"))
            step, clean = steps[index], steps[index + 1]
            with self.subTest(job=job_id):
                self.assertEqual(step["with"]["shared-key"], "${{ matrix.cache }}-${{ matrix.cache_key }}")
                self.assertEqual(step["with"]["add-rust-environment-hash-key"], "true")
                # A master run that saves without an exact hit starts from an empty target,
                # so a prefix restore never carries the old graph's builds into the new cache.
                save = step["with"]["save-if"].removeprefix("${{").removesuffix("}}").strip()
                self.assertIn(f"steps.{step['id']}.outputs.cache-hit != 'true'", clean["if"])
                self.assertIn(save, clean["if"])
                self.assertIn("matrix.cache_targets", clean["if"])
                self.assertEqual(clean["run"].splitlines(), [
                    "root=${WORKSPACES%% ->*} target=${WORKSPACES##*-> }", 'rm -rf "${root:?}/${target:?}"'])
                self.assertEqual(clean["env"]["WORKSPACES"], "${{ matrix.workspaces }}")
            families |= {f"{step['with']['prefix-key']}-{lane.cache}-" for lane in plan.LANES if lane.cache}
        # No lane family overlaps a release family or any other cache key, either way round.
        release = RESERVED_PREFIXES + ("ci-v1-agent-release-", "ci-v1-agent-package-")
        self.assertFalse([(f, r) for f in families for r in release if f.startswith(r) or r.startswith(f)])
        keys = [key for _, _, step in cache_steps()
                for key in [step["with"].get("key", "")] + str(step["with"].get("restore-keys", "")).splitlines()]
        keys += [f"{step['with'].get('prefix-key', 'v0-rust')}-{step['with'].get('shared-key') or step['with'].get('key', '')}"
                 for name, job_id, job in jobs() if (name, job_id) not in {("ci.yml", j) for j in plan.MATRIX_JOBS}
                 for step in job.get("steps", []) if step.get("uses", "").startswith("Swatinem/rust-cache@")]
        static = {key.strip().split("${{")[0] for key in keys} - {""}
        self.assertFalse([(f, k) for f in families for k in static if f.startswith(k) or k.startswith(f)])

    def test_c4_dev_testflight_build_number_is_the_run_number(self):
        job = workflows()["ios-dev-testflight.yml"]["jobs"]["build-ios-dev"]
        identity = next(step for step in job["steps"] if step.get("id") == "identity")
        self.assertEqual(identity["env"]["RUN_NUMBER"], "${{ github.run_number }}")

    def test_c5_eif_event_gate_matches_the_approval_policy(self):
        # Evaluate the real GitHub expressions with equivalent Python semantics. The
        # PR candidate is ci.yml's os-eif: the planner selects it, and its job repeats
        # the event, base-branch and head-repository checks. Its jobs need a successful
        # plan. opensecret-eif.yml's pull_request trigger accepts only PRs to master.
        approval = {job: expression(workflows()["opensecret-eif.yml"]["jobs"][job]) for job in ("eif", "eif-trusted")}
        candidate = expression(workflows()["ci.yml"]["jobs"]["os-eif"])
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
            ("merge_group", "refs/heads/gh-readonly-queue/master/pr-1", "success", "true", "false", False),
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
        self.assertEqual(workflows()["opensecret-eif.yml"]["on"]["pull_request"], {"branches": ["master"]})
        for case, (head, same_repo), base, cancelled in itertools.product(
                cases, heads, ("master", "feature"), (False, True)):
            event, ref, result, eif, approvals, expected = case
            runs = event != "pull_request" or base == "master"  # opensecret-eif.yml's trigger
            context = {
                "github.event_name": event, "github.ref": ref,
                "github.event.pull_request.base.ref": base,
                "github.event.pull_request.head.repo.full_name": head,
                "github.repository": "MaplePrivacyLabs/Maple", "needs.changes.result": result,
                "needs.changes.outputs.eif": eif, "needs.changes.outputs.pcr_approvals": approvals,
                "NOT_CANCELLED": not cancelled,
            }
            # The diff the planner classifies: an EIF input, an approval edit (itself an EIF input), or neither.
            paths = (["services/opensecret/src/main.rs"] if eif == "true" else []) + (
                ["services/opensecret/pcrDev.json"] if approvals == "true" else [])
            planned = "os-eif" in self.plan(event, head, paths, base)
            # The job's own condition holds the event, base and fork boundary even if the planner erred.
            self.assertEqual(evaluate(candidate, {"fromJSON(needs.plan.outputs.lanes)['os-eif']": True, **context}),
                             event == "pull_request" and base == "master" and same_repo, (case, head, base))
            selected = []
            if result == "success" and not cancelled and evaluate(candidate, {
                    "fromJSON(needs.plan.outputs.lanes)['os-eif']": planned, **context}):
                selected.append("os-eif")
            selected += [job for job, text in approval.items() if runs and evaluate(text, context)]
            trusted = ((event == "pull_request" and same_repo)
                       or (event in ("push", "workflow_dispatch") and ref == "refs/heads/master"))
            wanted = []
            if (not cancelled and event == "pull_request" and base == "master" and same_repo
                    and result == "success" and eif == "true" and approvals == "false"):
                wanted.append("os-eif")
            if expected and not cancelled and runs:
                wanted.append("eif-trusted" if trusted else "eif")
            self.assertEqual(selected, wanted, (case, head, base, cancelled))

    @staticmethod
    def plan(event: str, head, paths: list, base: str = "master") -> dict:
        """ci.yml's plan job for this event, base branch, head repository and diff."""
        payload = {"pull_request": {"number": 1, "base": {"sha": "a" * 40, "ref": base}, "head": {
            "sha": "b" * 40, "repo": None if head is None else {"full_name": head}}},
                   "merge_group": {"base_sha": "a" * 40, "head_sha": "b" * 40}, "before": "a" * 40, "after": "b" * 40}
        with mock.patch.object(plan, "changed_paths", return_value=paths), \
                mock.patch.dict(os.environ, {"GITHUB_REPOSITORY": "MaplePrivacyLabs/Maple"}), \
                mock.patch.dict(os.environ, {"GH_TOKEN": "", "GITHUB_TOKEN": ""}):
            try:
                return plan.plan_event(event, payload)
            except ValueError:  # an event ci.yml does not run on
                return {}

    def test_c5_eif_mismatch_fails_only_where_its_approval_is_edited(self):
        # Both approval jobs' compare step, run with a stand-in for check_opensecret_eif.sh
        # (exit 3 is a PCR mismatch). A mismatch fails a PR that edits that mode's
        # approval files, or a manual run outside master; on master it is a notice. Every
        # other failure fails.
        jobs_ = workflows()["opensecret-eif.yml"]["jobs"]
        name = "Build EIF and compare approved measurements"
        step, trusted = (next(s for s in jobs_[job]["steps"] if s.get("name") == name) for job in ("eif", "eif-trusted"))
        self.assertEqual(step, trusted)
        self.assertEqual(step["env"], {"PR_BASE_SHA": "${{ github.event.pull_request.base.sha }}",
                                       "PR_HEAD_SHA": "${{ github.event.pull_request.head.sha }}"})
        self.assertNotIn("${{", step["run"])  # event data reaches the script only through env
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            env = {key: value for key, value in os.environ.items() if not key.startswith("GIT_")}
            env.update(HOME=str(root), RUNNER_TEMP=str(root), GIT_CONFIG_NOSYSTEM="1",
                       GIT_CONFIG_GLOBAL=os.devnull)

            def git(*arguments):
                return subprocess.run(["git", "-c", "user.name=CI fixture", "-c", "user.email=ci@example.invalid",
                                       "-c", "core.hooksPath=/dev/null", *arguments], cwd=root, env=env,
                                      check=True, capture_output=True, text=True).stdout.strip()

            def commit(start, label, *edits):  # a distinct tree per label, so no two heads share a commit
                git("checkout", "-q", "--detach", start)
                for path in edits:
                    (root / path).write_text(f"{label}\n")
                git("add", "-A")
                git("commit", "-qm", label)
                return git("rev-parse", "HEAD")

            git("init", "-q")
            component = root / "services/opensecret"
            component.mkdir(parents=True)
            for file in ("pcrDev.json", "pcrDevHistory.json", "pcrProd.json", "pcrProdHistory.json", "main.rs"):
                (component / file).write_text("approved\n")
            stub = root / "scripts/ci/check_opensecret_eif.sh"
            stub.parent.mkdir(parents=True)
            stub.write_text('echo "Built measurements ($1, pcr.json):"; echo \'{"PCR0": "built"}\'; exit "$STATUS"\n')
            git("add", "-A")
            git("commit", "-qm", "base")
            base = git("rev-parse", "HEAD")
            # Master moves on with its own prod approval; only the PR's own edits count.
            master = commit(base, "master", "services/opensecret/pcrProd.json")
            heads = {label: commit(base, label, path) for label, path in (
                ("dev", "services/opensecret/pcrDevHistory.json"), ("prod", "services/opensecret/pcrProd.json"),
                ("code", "services/opensecret/main.rs"))}
            git("checkout", "-q", "--detach", base)
            git("rm", "-q", "services/opensecret/pcrDev.json")
            git("commit", "-qm", "delete")
            heads["deleted dev"] = git("rev-parse", "HEAD")
            self.assertEqual(len({master, *heads.values()}), 5)
            master_push, pr = ("push", "refs/heads/master", "", ""), ("pull_request", "refs/pull/1/merge", master)
            cases = [  # (event, ref, base, head), mode, outcome of a mismatch
                (master_push, "dev", "pending"), (master_push, "prod", "pending"),
                (("workflow_dispatch", "refs/heads/master", "", ""), "prod", "pending"),
                (("workflow_dispatch", "refs/heads/feature", "", ""), "dev", "fails"),
                (("workflow_dispatch", "refs/tags/v1", "", ""), "prod", "fails"),
                ((*pr, heads["dev"]), "dev", "fails"), ((*pr, heads["dev"]), "prod", "pending"),
                ((*pr, heads["prod"]), "prod", "fails"), ((*pr, heads["prod"]), "dev", "pending"),
                ((*pr, heads["code"]), "dev", "pending"), ((*pr, heads["deleted dev"]), "dev", "fails"),
                (("pull_request", "refs/pull/1/merge", "", heads["dev"]), "prod", "error"),
                (("pull_request", "refs/pull/1/merge", "f" * 40, heads["dev"]), "prod", "error"),
                # Only full commit ids: an option or an abbreviation would make a real edit look absent.
                (("pull_request", "refs/pull/1/merge", f"--output={root / 'diff.out'}", heads["prod"]), "prod", "error"),
                ((*pr[:2], master[:12], heads["prod"]), "prod", "error"),
                ((*pr, f"--output={root / 'diff.out'}"), "prod", "error"),
            ]
            script = root / "step.sh"
            script.write_text(step["run"])
            for (event, ref, pr_base, pr_head), mode, outcome in cases:
                for status in (0, 1, 2, 3, 42):
                    summary = root / "summary.md"
                    summary.write_text("")
                    result = subprocess.run(
                        [shutil.which("bash"), "--noprofile", "--norc", "-eo", "pipefail", str(script)], cwd=root,
                        env={**env, "GITHUB_EVENT_NAME": event, "GITHUB_REF": ref, "PR_BASE_SHA": pr_base,
                             "PR_HEAD_SHA": pr_head, "EIF_MODE": mode, "STATUS": str(status),
                             "GITHUB_STEP_SUMMARY": str(summary)}, capture_output=True, text=True)
                    with self.subTest(event=event, ref=ref, head=pr_head, mode=mode, status=status):
                        self.assertIn('{"PCR0": "built"}', result.stdout)  # the check's output stays in the log
                        if status != 3:
                            self.assertEqual(result.returncode, status)
                            self.assertEqual(summary.read_text(), "")
                            continue
                        expected = {"pending": 0, "fails": 3, "error": 1}[outcome]
                        self.assertEqual(result.returncode, expected, result.stdout + result.stderr)
                        self.assertEqual("::notice title=EIF/PCR approval pending" in result.stdout,
                                         outcome == "pending")
                        if outcome != "error":
                            self.assertIn(f"### EIF/PCR approval {'pending' if expected == 0 else 'mismatch'}"
                                          f" ({mode})", summary.read_text())
                            self.assertIn('{"PCR0": "built"}', summary.read_text())
            self.assertEqual(list(root.glob("diff.out*")), [])  # git never read a SHA as an option

    def test_c5_oidc_and_flakehub_stay_in_their_jobs_on_one_builder_class(self):
        minted = {(name, job_id) for name, job_id, job in jobs()
                  if permissions(workflows()[name], job).get("id-token") == "write"}
        self.assertEqual(minted, OIDC_JOBS)
        builder = workflows()["opensecret-eif-release.yml"]["jobs"]["build"]["runs-on"]
        flakehub = {(name, job_id) for name, job_id, job in jobs() for step in job.get("steps", [])
                    if "flakehub-cache-action@" in step.get("uses", "")}
        self.assertEqual(flakehub, FLAKEHUB_JOBS)
        for name, job_id in flakehub:  # every EIF builder, on the release builder's runner class
            self.assertEqual(workflows()[name]["jobs"][job_id]["runs-on"], builder, job_id)
        self.assertEqual(next(lane for lane in plan.LANES if lane.id == "os-eif").runner.label, builder)

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
        titles = {config.get("name") for config in workflows().values()}  # display names
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
            # A quoted or bold name before "workflow" is a display name, unless it is a path or file.
            for match in re.finditer(r"(?:`([^`\n]+)`|\*\*([^*\n]+)\*\*)\s+workflows?\b", text):
                title = match.group(1) or match.group(2)
                if "/" not in title and not re.search(r"\.ya?ml$", title) and title not in titles:
                    dangling.append(f"{path}: {title}")
        self.assertEqual(dangling, [])


if __name__ == "__main__":
    unittest.main()
