"""Invariants every GitHub Actions workflow keeps (docs/ci.md)."""

from __future__ import annotations

import functools
import json
from pathlib import Path
import re
import subprocess
import unittest

import lanes

ROOT = Path(__file__).resolve().parents[2]
WORKFLOWS = ROOT / ".github/workflows"
# Release and publication workflows that stay byte-identical; owner review only.
SACRED = frozenset({
    "release.yml", "pages-publish.yml", "proxy-publish.yml", "updates-publish.yml", "zapstore-publish.yml",
    "sdk-publish-npm.yml", "sdk-publish-rust.yml", "agent-updates-publish.yml", "auth-pages-publish.yml",
    "opensecret-eif-release.yml",
})
LISTENERS = frozenset({
    "auth-pages-dev-publish.yml", "pages-publish.yml", "proxy-publish.yml", "updates-publish.yml",
    "zapstore-publish.yml",
})
NIX_ACTION = "DeterminateSystems/determinate-nix-action@96c5ee93a9dddf2830d31e1da6ec719077336c28"
NIX_INPUTS = {"source-tag": "v3.23.3", "github-token": ""}
VERIFY_NIX = "bash scripts/ci/verify-nix-runtime.sh"
SETUP_NIX = "./.github/actions/setup-nix"
PINNED = re.compile(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+(?:/[A-Za-z0-9_./-]+)?@[0-9a-f]{40}")
# Standard images, and the org's one GitHub-hosted larger runner, by its exact label:
# a pattern would also admit a self-hosted runner that borrowed the name.
# GitHub-hosted labels: the standard images, the per-Xcode macOS images (xcode-27) and the
# org's larger Windows runner.
GITHUB_HOSTED = re.compile(r"(?:ubuntu|macos|windows)-[A-Za-z0-9.-]+|xcode-[0-9]+|maple-windows-2025-16core")
# The workflows that run on pull requests or the merge queue. Every other check is a
# ci.yml lane (scripts/ci/lanes.py), so ci-ok judges it; adding one here is an owner call.
PULL_REQUEST_WORKFLOWS = frozenset({
    "ci.yml", "opensecret-eif.yml", "pages-preview-build.yml", "auth-pages-ci.yml", "proxy-rust.yml",
})
CLOUDFLARE = frozenset({"CLOUDFLARE_ACCOUNT_ID", "CLOUDFLARE_API_TOKEN"})
# Every protected environment and the secrets configured in it.
ENVIRONMENTS = {
    "desktop-signing": frozenset({
        "TAURI_SIGNING_PRIVATE_KEY", "TAURI_SIGNING_PRIVATE_KEY_PASSWORD", "APPLE_CERTIFICATE",
        "APPLE_CERTIFICATE_PASSWORD", "APPLE_ID", "APPLE_ID_PASSWORD", "APPLE_TEAM_ID", "KEYCHAIN_PASSWORD",
    }),
    "apple-signing": frozenset({"APPLE_API_ISSUER", "APPLE_API_KEY", "APPLE_API_PRIVATE_KEY", "APPLE_TEAM_ID"}),
    "android-signing": frozenset({"ANDROID_KEYSTORE_BASE64", "ANDROID_KEY_ALIAS", "ANDROID_KEY_PASSWORD"}),
    "windows-signing": frozenset({
        "TAURI_SIGNING_PRIVATE_KEY", "TAURI_SIGNING_PRIVATE_KEY_PASSWORD", "AZURE_ACCOUNT",
        "AZURE_ARTIFACT_SIGNING_ACCOUNT_NAME", "AZURE_ARTIFACT_SIGNING_CERTIFICATE_PROFILE_NAME",
        "AZURE_ARTIFACT_SIGNING_ENDPOINT", "AZURE_ARTIFACT_SIGNING_EXPECTED_SUBJECT", "AZURE_CLIENT_ID",
        "AZURE_ENDPOINT", "AZURE_PROFILE", "AZURE_SUBSCRIPTION_ID", "AZURE_TENANT_ID",
    }),
    "zapstore-publishing": frozenset({"ZAPSTORE_SIGN_WITH"}),
    "updates-production": frozenset({"AGENT_UPDATES_BWS_ACCESS_TOKEN"}) | CLOUDFLARE,
    "pages-preview": CLOUDFLARE,
    "pages-production": CLOUDFLARE,
    "auth-pages-development": CLOUDFLARE,
    "auth-pages-production": CLOUDFLARE,
    "pcr-signing": frozenset({"OPENSECRET_PCR_SIGNING_BWS_ACCESS_TOKEN"}),
    "sdk-npm": frozenset({"NPM_BOOTSTRAP_TOKEN"}),
    "sdk-crates": frozenset({"CRATES_BOOTSTRAP_TOKEN"}),
}
# Actions-cache writers; a key under these prefixes feeds release.yml's fallbacks.
# setup-bun's post step saves the Bun binary unless `no-cache` is set; magic-nix-cache
# saves the Nix store to the Actions cache unless `use-gha-cache` is disabled.
CACHE_WRITERS = ("actions/cache@", "actions/cache/save@", "irgaly/xcode-cache@", "Swatinem/rust-cache@",
                 "oven-sh/setup-bun@", "DeterminateSystems/magic-nix-cache-action@")
# The one pull-request job that saves a cache: the EIF approval check for fork PRs
# that edit approvals and for manual runs outside master. It has no FlakeHub
# credentials, so its Nix store goes to the run's own branch-scoped Actions cache,
# which master and other pull requests cannot read.
BRANCH_SCOPED_NIX = frozenset({("opensecret-eif.yml", "eif")})
# Jobs on GitHub-hosted runners that save caches into GitHub's store; every other
# cache lives in Blacksmith's. The release and the Nix installer also use GitHub's.
GITHUB_STORE_WRITERS = frozenset({
    ("desktop-build.yml", "build-windows"),  # the signed Research Windows build (setup-bun's Bun binary)
    # The Agent lanes on GitHub's Windows image: the native suites in the windows
    # matrix and the packaging rehearsal.
    ("ci.yml", "windows"), ("ci.yml", "agent-package-windows"),
}) | BRANCH_SCOPED_NIX
# The only cached matrix lanes whose home runner uses GitHub's store.
GITHUB_STORE_LANES = frozenset({"agent-full-windows", "agent-windows-updater"})
# A special job whose runner a repository variable overrides saves only on its home
# runner's vendor; Blacksmith runner names start with "blacksmith".
ON_BLACKSMITH = "startsWith(runner.name, 'blacksmith')"

@functools.lru_cache(maxsize=None)
def load(path: Path) -> dict:
    return json.loads(subprocess.run(["yq", "-o=json", ".", str(path)], check=True,
                                     capture_output=True, text=True).stdout)


def workflows() -> dict:
    return {path.name: load(path) for path in sorted(WORKFLOWS.glob("*.yml"))}


def jobs():
    for name, config in workflows().items():
        for job_id, job in config["jobs"].items():
            yield name, job_id, job


def triggers(config: dict) -> set:
    on = config["on"]
    return set(on) if isinstance(on, dict) else set([on] if isinstance(on, str) else on)


def environment(job: dict):
    value = job.get("environment")
    return value.get("name") if isinstance(value, dict) else value


def secrets(value) -> set:
    return set(re.findall(r"secrets\.([A-Za-z0-9_]+)", json.dumps(value))) - {"GITHUB_TOKEN"}


def check_secret_boundary(job: dict) -> None:
    used = secrets(job)
    if used and not (isinstance(environment(job), str) and used <= ENVIRONMENTS.get(environment(job), set())):
        raise ValueError(f"secrets {sorted(used)} need a fixed protected environment that holds them")


def permissions(config: dict, job: dict) -> dict:
    value = job.get("permissions", config.get("permissions", {}))
    return value if isinstance(value, dict) else {}


def matrix_values(matrix, key: str) -> set:
    if isinstance(matrix, str):
        return set(re.findall(rf'"{re.escape(key)}"\s*:\s*"([^"]+)"', matrix))
    values = matrix.get(key, [])
    found = set(values) if isinstance(values, list) else matrix_values(values, key)
    include = matrix.get("include", [])
    if isinstance(include, str):
        return found | matrix_values(include, key)
    return found | {row[key] for row in include if key in row}


def runner_labels(name: str, job_id: str, job: dict) -> set:
    if name == "ci.yml" and job_id in lanes.MATRIX_JOBS:  # the planner fills these matrices from lanes.py
        return {lane.runner.label for lane in lanes.LANES if lane.job == job_id}
    values = job.get("runs-on", [])
    labels = set()
    for value in values if isinstance(values, list) else [values]:
        if "${{" not in value:
            labels.add(value)
            continue
        labels |= set(re.findall(r"'([^']+)'", value))
        for key in re.findall(r"matrix\.([A-Za-z0-9_-]+)", value):
            labels |= matrix_values(job.get("strategy", {}).get("matrix", {}), key)
    return labels


def cache_writer(step: dict) -> bool:
    settings = step.get("with", {})
    return (step.get("uses", "").startswith(CACHE_WRITERS) and str(settings.get("save-if", "true")) != "false"
            and str(settings.get("no-cache", "false")).lower() != "true"
            and str(settings.get("use-gha-cache", "enabled")).lower() != "disabled")


class WorkflowPolicyTests(unittest.TestCase):
    def test_1_no_privileged_pull_request_or_unlisted_workflow_run_triggers(self):
        for name, config in workflows().items():
            with self.subTest(workflow=name):
                self.assertNotIn("pull_request_target", triggers(config))
                if "workflow_run" in triggers(config):
                    self.assertIn(name, LISTENERS)
        self.assertEqual({name for name, config in workflows().items()
                          if {"pull_request", "merge_group"} & triggers(config)}, PULL_REQUEST_WORKFLOWS)

    def test_2_every_action_is_local_or_pinned_to_a_commit(self):
        files = sorted(WORKFLOWS.glob("*.yml")) + sorted((ROOT / ".github/actions").glob("*/action.yml"))
        for path in files:
            config = load(path)
            units = list(config.get("jobs", {}).values()) + [config.get("runs", {})]
            for unit in units:
                for uses in [unit.get("uses")] + [step.get("uses") for step in unit.get("steps", [])]:
                    if uses:
                        with self.subTest(file=path.name, uses=uses):
                            self.assertTrue(uses.startswith("./") or PINNED.fullmatch(uses))

    def test_3_checkouts_never_persist_credentials(self):
        for name, job_id, job in jobs():
            for step in job.get("steps", []):
                if step.get("uses", "").startswith("actions/checkout@"):
                    with self.subTest(workflow=name, job=job_id):
                        self.assertIs(step.get("with", {}).get("persist-credentials"), False)

    def test_4_workflow_permissions_are_read_only(self):
        # Write scopes are granted per job, to the jobs that need them.
        for name, config in workflows().items():
            granted = config.get("permissions")
            with self.subTest(workflow=name):
                self.assertIsInstance(granted, dict)
                if name in SACRED:
                    self.assertNotIn("write", granted.values())
                else:
                    self.assertEqual(granted, {"contents": "read"})

    def test_5_secrets_are_used_only_inside_their_protected_environment(self):
        for name, config in workflows().items():
            with self.subTest(workflow=name):
                self.assertFalse(secrets({key: value for key, value in config.items() if key != "jobs"}))
                for job_id, job in config["jobs"].items():
                    self.assertNotEqual(job.get("secrets"), "inherit", job_id)
                    check_secret_boundary(job)
        reference = {"steps": [{"env": {"KEY": "${{ secrets.APPLE_API_PRIVATE_KEY }}"}}]}
        for wrong in (None, "pages-preview", "${{ inputs.environment }}", "android-signing"):
            with self.subTest(environment=wrong), self.assertRaises(ValueError):
                check_secret_boundary({**reference, "environment": wrong})
        check_secret_boundary({**reference, "environment": "apple-signing"})

    def test_6_every_nix_install_is_pinned_and_verified(self):
        composite = load(ROOT / ".github/actions/setup-nix/action.yml")["runs"]["steps"]
        self.assertEqual([(step.get("uses"), step.get("with"), step.get("run")) for step in composite],
                         [(NIX_ACTION, NIX_INPUTS, None), (None, None, VERIFY_NIX)])
        for name, config in workflows().items():
            root = config.get("defaults", {}).get("run", {}).get("working-directory", ".")
            for job_id, job in config["jobs"].items():
                directory = job.get("defaults", {}).get("run", {}).get("working-directory", root)
                steps = job.get("steps", [])
                for index, step in enumerate(steps):
                    uses = step.get("uses", "")
                    if not uses.startswith(("DeterminateSystems/determinate-nix-action@",
                                            "DeterminateSystems/nix-installer-action@", SETUP_NIX)):
                        continue
                    with self.subTest(workflow=name, job=job_id):
                        self.assertTrue(any(s.get("uses", "").startswith("actions/checkout@") for s in steps[:index]))
                        if uses == SETUP_NIX:
                            continue
                        self.assertEqual((uses, step.get("with")), (NIX_ACTION, NIX_INPUTS))
                        verify = steps[index + 1] if index + 1 < len(steps) else {}
                        self.assertEqual(verify.get("run"), VERIFY_NIX)
                        self.assertEqual(verify.get("if"), step.get("if"))
                        self.assertEqual(verify.get("working-directory", directory), ".")

    def test_7_every_job_has_a_timeout(self):
        missing = {(name, job_id) for name, job_id, job in jobs()
                   if name not in SACRED and "uses" not in job and "timeout-minutes" not in job}
        self.assertEqual(missing, set())

    def test_8_ci_always_reports_one_verdict(self):
        config = workflows()["ci.yml"]
        for event, settings in config["on"].items():
            self.assertFalse(isinstance(settings, dict) and {"paths", "paths-ignore"} & set(settings), event)
        verdict = config["jobs"]["ci-ok"]
        self.assertEqual(verdict["if"], "always()")
        self.assertEqual(set(verdict["needs"]), set(config["jobs"]) - {"ci-ok"})
        # nightly.yml's report likewise needs every other nightly job.
        nightly = workflows()["nightly.yml"]["jobs"]
        self.assertEqual(set(nightly["report"]["needs"]), set(nightly) - {"report"})

    def test_9_runner_labels_are_github_hosted_or_declared(self):
        declared = set(load(ROOT / ".github/actionlint.yaml")["self-hosted-runner"]["labels"])
        for name, job_id, job in jobs():
            if "uses" in job:
                continue
            labels = runner_labels(name, job_id, job)
            with self.subTest(workflow=name, job=job_id):
                self.assertTrue(labels, "runs-on must name its labels")
                for label in labels:
                    self.assertTrue(GITHUB_HOSTED.fullmatch(label) or label in declared, label)

    def test_pull_requests_never_save_caches_or_read_other_runs_artifacts(self):
        scoped = set()
        for name, config in workflows().items():
            pr = bool({"pull_request", "merge_group"} & triggers(config))
            for job_id, job in config["jobs"].items():
                for step in job.get("steps", []):
                    self.assertNotIn("run-id", step.get("with", {}), (name, job_id))
                    guard = " ".join(str(value) for value in (
                        job.get("if", ""), step.get("if", ""), step.get("with", {}).get("save-if", "")))
                    if pr and cache_writer(step) and (name, job_id) in BRANCH_SCOPED_NIX:
                        scoped.add((name, job_id))
                        self.assertTrue(step["uses"].startswith("DeterminateSystems/magic-nix-cache-action@"))
                        self.assertEqual(step["with"].get("use-flakehub"), "disabled")
                        self.assertNotIn("id-token", json.dumps(job))
                    elif pr and cache_writer(step):
                        self.assertRegex(guard, r"refs/heads/master|github\.event_name == 'push'", (name, job_id))
                        if name == "ci.yml":  # master's pushes and nightly save; dispatch and queue runs only restore
                            events = set(re.findall(r"github\.event_name == '(\w+)'", guard))
                            self.assertTrue(events and events <= {"push", "schedule"}, (job_id, guard))
        self.assertEqual(scoped, BRANCH_SCOPED_NIX)  # the exemption lapses with its job

    def test_only_listed_jobs_fill_githubs_cache_store_and_failovers_never_save(self):
        # Blacksmith runners cannot read GitHub's store, nor GitHub's runners Blacksmith's.
        writers = set()
        for name, job_id, job in jobs():
            for step in job.get("steps", []):
                if name in SACRED or not cache_writer(step):
                    continue
                guard = " ".join(str(value) for value in (step.get("if", ""), step["with"].get("save-if", "")))
                if name == "ci.yml" and job_id in lanes.MATRIX_JOBS:
                    # The planner's cache_save is false when a failover leaves the lane's home vendor.
                    self.assertIn("matrix.cache_save", guard, job_id)
                    labels = {lane.runner.label for lane in lanes.LANES if lane.job == job_id and lane.cache}
                else:
                    labels = runner_labels(name, job_id, job)
                    if "vars." in str(job.get("runs-on")):  # the default label is the home runner
                        with self.subTest(workflow=name, job=job_id):
                            self.assertIn(ON_BLACKSMITH, guard)
                            self.assertEqual("!" + ON_BLACKSMITH in guard,
                                             {lanes.cache_store(label) for label in labels} == {"github"})
                if "github" in {lanes.cache_store(label) for label in labels}:  # any non-Blacksmith runner
                    writers.add((name, job_id))
        self.assertEqual(writers, GITHUB_STORE_WRITERS)
        # Within a matrix writer, pin the lanes themselves.
        self.assertEqual({lane.id for lane in lanes.LANES if lane.job in lanes.MATRIX_JOBS and lane.cache
                          and lanes.cache_store(lane.runner.label) == "github"}, GITHUB_STORE_LANES)

if __name__ == "__main__":
    unittest.main()
