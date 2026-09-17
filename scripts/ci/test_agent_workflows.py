"""Security, component selection, and Research release isolation for Agent CI."""

import functools
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import tomllib
import unittest


ROOT = Path(__file__).resolve().parents[2]


@functools.cache
def workflow(name):
    result = subprocess.run(
        ["yq", "-o=json", ".", str(ROOT / ".github" / "workflows" / name)],
        check=True, capture_output=True, text=True,
    )
    return json.loads(result.stdout)


def strings(value):
    if isinstance(value, str):
        yield value
    elif isinstance(value, dict):
        for key, child in value.items():
            yield str(key)
            yield from strings(child)
    elif isinstance(value, list):
        for child in value:
            yield from strings(child)


class AgentWorkflowBoundaryTests(unittest.TestCase):
    def test_contributor_build_has_no_credentials_or_privileged_events(self):
        config = workflow("agent-ci.yml")
        self.assertEqual(set(config["on"]), {"push", "pull_request", "workflow_dispatch"})
        self.assertEqual(config["on"]["push"]["branches"], ["master"])
        self.assertEqual(config["on"]["pull_request"]["branches"], ["master"])
        self.assertEqual(config["permissions"], {"contents": "read"})
        for value in strings(config):
            self.assertNotRegex(value, r"\bsecrets\b|github\.token|\bGH_TOKEN\b")
        for job in config["jobs"].values():
            self.assertNotIn("environment", job)
            self.assertNotIn("uses", job)
            self.assertIn(job.get("permissions"), (None, {"contents": "read"}))
            for step in job["steps"]:
                self.assertNotIn("${{", step.get("run", ""))
                action = step.get("uses", "")
                if action:
                    self.assertRegex(action, r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+@[0-9a-f]{40}$")
                if action.startswith("actions/checkout@"):
                    self.assertIs(step["with"]["persist-credentials"], False)
                if action.startswith("DeterminateSystems/nix-installer-action@"):
                    self.assertEqual(step["with"]["github-token"], "")

    def test_agent_cache_and_artifacts_do_not_share_research_publication(self):
        steps = workflow("agent-ci.yml")["jobs"]["desktop"]["steps"]
        caches = [step["with"] for step in steps if "rust-cache@" in step.get("uses", "")]
        self.assertTrue(caches)
        for cache in caches:
            self.assertEqual(cache["workspaces"], "apps/maple-agent -> target")
            self.assertTrue(cache["key"].startswith("maple-agent-"))
            self.assertEqual(cache["save-if"],
                             "${{ github.event_name == 'push' && github.ref == 'refs/heads/master' }}")
        for step in steps:
            action = step.get("uses", "")
            self.assertNotIn("release", action.lower())
            self.assertNotIn("download-artifact", action)
            if "upload-artifact@" in action:
                self.assertTrue(step["with"]["name"].startswith("maple-agent-"))
                self.assertTrue(step["with"]["path"].startswith("apps/maple-agent/target/"))
        self.assertFalse(any((ROOT / "apps/maple-agent/.github/workflows").glob("*.yml")))

    def test_failed_or_missing_selection_cannot_skip_the_desktop_matrix(self):
        condition = workflow("agent-ci.yml")["jobs"]["desktop"]["if"]
        self.assertIn("always() && !cancelled()", condition)
        self.assertIn("needs.changes.result != 'success'", condition)
        self.assertIn("needs.changes.outputs.agent != 'false'", condition)
        self.assertNotIn("head.repo", condition)

    def test_namespaced_agent_releases_do_not_enter_research_jobs(self):
        release = workflow("release.yml")
        classifier = release["jobs"]["classify-app-release"]
        self.assertEqual(classifier["if"], "startsWith(github.event.release.tag_name, 'v')")
        # All release work remains downstream of the classifier, with GitHub's
        # default success gate. Skipping it skips builds and publishers together.
        for name, job in release["jobs"].items():
            if name == "classify-app-release":
                continue
            needs = job["needs"]
            if isinstance(needs, str):
                needs = [needs]
            self.assertIn("classify-app-release", needs)
            self.assertNotIn("if", job)
        for name, job_name in (
            ("pages-publish.yml", "production"),
            ("pages-production.yml", "promote"),
            ("updates-publish.yml", "publish"),
            ("proxy-publish.yml", "prepare"),
            ("zapstore-publish.yml", "publish"),
        ):
            with self.subTest(workflow=name):
                condition = workflow(name)["jobs"][job_name]["if"]
                self.assertIn("startsWith(github.event.workflow_run.head_branch, 'v') &&", condition)
                self.assertIn("github.event.workflow_run.conclusion == 'success'", condition)
                self.assertIn("github.event.workflow_run.event == 'release'", condition)


class AgentSupplyChainTests(unittest.TestCase):
    def test_scan_has_no_credentials_publication_or_privileged_events(self):
        config = workflow("agent-supply-chain.yml")
        self.assertEqual(set(config["on"]),
                         {"push", "pull_request", "schedule", "workflow_dispatch"})
        self.assertEqual(config["permissions"], {"contents": "read"})
        for value in strings(config):
            self.assertNotRegex(value, r"\bsecrets\b|github\.token|\bGH_TOKEN\b")
        self.assertEqual(set(config["jobs"]), {"agent-cargo-deny"})
        job = config["jobs"]["agent-cargo-deny"]
        self.assertEqual(job["runs-on"], "ubuntu-latest")
        self.assertLessEqual(job["timeout-minutes"], 10)
        self.assertNotIn("environment", job)
        self.assertNotIn("permissions", job)
        self.assertNotIn("continue-on-error", job)
        self.assertEqual(len(job["steps"]), 2)
        for step in job["steps"]:
            self.assertNotIn("run", step)
            self.assertNotIn("continue-on-error", step)
            self.assertRegex(step["uses"],
                             r"^(actions/checkout|EmbarkStudios/cargo-deny-action)@[0-9a-f]{40}$")
        self.assertIs(job["steps"][0]["with"]["persist-credentials"], False)
        scan = job["steps"][1]["with"]
        self.assertEqual(scan, {
            "rust-version": "1.98.0",
            "manifest-path": "apps/maple-agent/Cargo.toml",
            "command": "check advisories bans",
            "arguments": "--config apps/maple-agent/deny.toml --all-features --locked",
        })

    def test_scan_covers_each_dependency_manifest_and_the_agent_lock(self):
        config = workflow("agent-supply-chain.yml")
        self.assertEqual(config["on"]["push"]["branches"], ["master"])
        paths = config["on"]["pull_request"]["paths"]
        self.assertEqual(paths, config["on"]["push"]["paths"])
        self.assertEqual(set(paths), {
            ".github/workflows/agent-supply-chain.yml",
            "apps/maple-agent/deny.toml", "apps/maple-agent/**/Cargo.toml",
            "apps/maple-agent/Cargo.lock", "proxy/Cargo.toml", "sdk/rust/Cargo.toml",
        })
        self.assertEqual(len(config["on"]["schedule"]), 1)
        self.assertRegex(config["on"]["schedule"][0]["cron"], r"^\d+ \d+ \* \* \*$")

    def test_agent_policy_does_not_inherit_advisory_exceptions(self):
        config = tomllib.loads((ROOT / "apps/maple-agent/deny.toml").read_text())
        self.assertEqual(config["advisories"], {"unsound": "all", "unmaintained": "all"})
        # Incident containment applies to the new component too. An intentional
        # policy update must reconcile both lists rather than accidentally omit it.
        sdk = tomllib.loads((ROOT / "sdk/deny.toml").read_text())
        self.assertEqual(config["bans"]["deny"], sdk["bans"]["deny"])


class AgentDiffSelectionTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.git("init", "-q", "-b", "master")
        self.git("config", "user.email", "ci@example.invalid")
        self.git("config", "user.name", "CI fixture")
        self.git("config", "core.hooksPath", "/dev/null")
        scripts = self.root / "scripts/ci"
        scripts.mkdir(parents=True)
        for name in ("agent_change_detection.py", "change_detection.py"):
            shutil.copyfile(ROOT / "scripts/ci" / name, scripts / name)
        self.base = self.commit_file("README.md", "initial\n")

    def git(self, *arguments):
        result = subprocess.run(["git", *arguments], cwd=self.root, check=True,
                                capture_output=True, text=True)
        return result.stdout.strip()

    def commit_file(self, path, text):
        file = self.root / path
        file.parent.mkdir(parents=True, exist_ok=True)
        file.write_text(text)
        self.git("add", "--", path)
        self.git("commit", "-qm", "fixture change")
        return self.git("rev-parse", "HEAD")

    def select(self, event, base, head):
        step = next(step for step in workflow("agent-ci.yml")["jobs"]["changes"]["steps"]
                    if step.get("id") == "classify")
        output = self.root / "output"
        output.unlink(missing_ok=True)
        env = {**os.environ, "GITHUB_EVENT_NAME": event, "BASE_SHA": base, "HEAD_SHA": head,
               "GITHUB_OUTPUT": str(output)}
        result = subprocess.run(["bash", "-c", step["run"]], cwd=self.root, env=env,
                                capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        return output.read_text()

    def test_docs_only_push_and_agent_runtime_push(self):
        docs = self.commit_file("apps/maple-agent/docs/design.md", "design\n")
        self.assertEqual(self.select("push", self.base, docs), "agent=false\n")
        runtime = self.commit_file("apps/maple-agent/app/src/main.rs", "fn main() {}\n")
        self.assertEqual(self.select("push", docs, runtime), "agent=true\n")

    def test_pull_request_uses_merge_base_instead_of_unrelated_base_changes(self):
        master = self.commit_file("apps/maple-agent/app/src/main.rs", "fn main() {}\n")
        self.git("checkout", "-qb", "contributor", self.base)
        docs = self.commit_file("apps/maple-agent/docs/design.md", "design\n")
        self.assertEqual(self.select("pull_request", master, docs), "agent=false\n")

    def test_deletion_or_rename_out_of_component_still_builds(self):
        runtime = self.commit_file("apps/maple-agent/app/src/main.rs", "fn main() {}\n")
        self.git("mv", "apps/maple-agent/app/src/main.rs", "README.md.moved")
        self.git("commit", "-qm", "rename fixture")
        self.assertEqual(self.select("push", runtime, self.git("rev-parse", "HEAD")), "agent=true\n")

    def test_missing_history_manual_event_and_classifier_failure_select_build(self):
        for event, base, head in (
            ("push", "0" * 40, self.base),
            ("push", "a" * 40, self.base),
            ("workflow_dispatch", "", ""),
        ):
            with self.subTest(event=event, base=base):
                self.assertEqual(self.select(event, base, head), "agent=true\n")
        (self.root / "scripts/ci/agent_change_detection.py").write_text("raise RuntimeError('fixture')\n")
        self.assertEqual(self.select("push", self.base, self.base), "agent=true\n")


if __name__ == "__main__":
    unittest.main()
