"""Invariants every GitHub Actions workflow keeps (docs/repository-workflows.md).

TEMPORARY lists the violations that later commits in this change remove. Each
list must equal the current violations exactly, so an entry is deleted as soon
as its violation is fixed and no new violation can hide behind it.
"""

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
GITHUB_HOSTED = re.compile(r"(?:ubuntu|macos|windows)-[A-Za-z0-9.-]+")
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
CACHE_WRITERS = ("actions/cache@", "actions/cache/save@", "irgaly/xcode-cache@", "Swatinem/rust-cache@")

TEMPORARY = {
    # Signed-lane hygiene adds the timeouts and moves OIDC to the attesting jobs.
    "timeout": frozenset({
        ("android-build.yml", "build-android"), ("android-build.yml", "verify-android-artifacts"),
        ("app-change-detection.yml", "detect"),
        *(("desktop-build.yml", job) for job in (
            "build-macos", "build-linux", "verify-linux-desktop-artifacts", "verify-macos-desktop-artifacts",
            "build-windows", "verify-windows-desktop-artifacts", "verify-desktop-artifacts")),
        ("mobile-build.yml", "build-ios"), ("mobile-build.yml", "verify-ios-artifacts"),
        ("mobile-build.yml", "submit-ios-testflight"),
        *(("signing-credentials-check.yml", job) for job in (
            "desktop-signing", "apple-signing", "android-signing", "windows-signing", "zapstore-publishing")),
        ("web-build.yml", "build-web"), ("web-build.yml", "build-web-master"),
        ("web-build.yml", "verify-web-master-artifact"),
        # Deleted, or made push-only, by the client lanes.
        ("android-pr-build.yml", "build-android"), ("mobile-pr-build.yml", "build-ios"),
        *(("desktop-pr-build.yml", job) for job in (
            "build-macos", "build-research-dev-macos", "build-linux", "build-windows", "verify-windows-artifacts")),
        ("frontend-tests.yml", "test-frontend"), ("rust-tests.yml", "rust-tests"),
    }),
    "permissions": frozenset({"desktop-build.yml", "android-build.yml"}),
    # The client lanes delete these PR workflows or make them push-only.
    "pr-cache-writes": frozenset({
        ("android-pr-build.yml", "build-android"), ("desktop-pr-build.yml", "build-macos"),
        ("desktop-pr-build.yml", "build-research-dev-macos"), ("desktop-pr-build.yml", "build-linux"),
        ("mobile-pr-build.yml", "build-ios"), ("frontend-tests.yml", "test-frontend"),
        ("rust-tests.yml", "rust-tests"), ("web-build.yml", "build-web"), ("proxy-rust.yml", "proxy-rust"),
        ("proxy-rust.yml", "proxy-native-release"), ("sdk-rust.yml", "sdk-rust"),
        ("sdk-typescript.yml", "sdk-typescript"), ("updates-tests.yml", "test-updates-worker"),
    }),
}


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
    uses = step.get("uses", "")
    return uses.startswith(CACHE_WRITERS) and str(step.get("with", {}).get("save-if", "true")) != "false"


class WorkflowPolicyTests(unittest.TestCase):
    def test_1_no_privileged_pull_request_or_unlisted_workflow_run_triggers(self):
        for name, config in workflows().items():
            with self.subTest(workflow=name):
                self.assertNotIn("pull_request_target", triggers(config))
                if "workflow_run" in triggers(config):
                    self.assertIn(name, LISTENERS)

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
        broad = set()
        for name, config in workflows().items():
            granted = config.get("permissions")
            with self.subTest(workflow=name):
                self.assertIsInstance(granted, dict)
                if name in SACRED:
                    self.assertNotIn("write", granted.values())
                elif granted != {"contents": "read"}:
                    broad.add(name)
        self.assertEqual(broad, TEMPORARY["permissions"])

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
        self.assertEqual(missing, TEMPORARY["timeout"])

    def test_8_ci_always_reports_one_verdict(self):
        config = workflows()["ci.yml"]
        for event, settings in config["on"].items():
            self.assertFalse(isinstance(settings, dict) and {"paths", "paths-ignore"} & set(settings), event)
        verdict = config["jobs"]["ci-ok"]
        self.assertEqual(verdict["if"], "always()")
        self.assertEqual(set(verdict["needs"]), set(config["jobs"]) - {"ci-ok"})

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
        writers = set()
        for name, config in workflows().items():
            pr = bool({"pull_request", "merge_group"} & triggers(config))
            for job_id, job in config["jobs"].items():
                for step in job.get("steps", []):
                    self.assertNotIn("run-id", step.get("with", {}), (name, job_id))
                    guard = " ".join(str(value) for value in (
                        job.get("if", ""), step.get("if", ""), step.get("with", {}).get("save-if", "")))
                    if pr and cache_writer(step) and not re.search(
                            r"refs/heads/master|github\.event_name == 'push'", guard):
                        writers.add((name, job_id))
        self.assertEqual(writers, TEMPORARY["pr-cache-writes"])


if __name__ == "__main__":
    unittest.main()
