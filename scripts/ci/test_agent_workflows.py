"""Security, component selection, and Research release isolation for Agent CI."""

import functools
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
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

    def test_failed_or_missing_selection_cannot_skip_agent_validation(self):
        jobs = workflow("agent-ci.yml")["jobs"]
        for name in ("desktop", "nix-python"):
            with self.subTest(job=name):
                self.assertEqual(jobs[name]["needs"], "changes")
                condition = jobs[name]["if"]
                self.assertIn("always() && !cancelled()", condition)
                self.assertIn("needs.changes.result != 'success'", condition)
                self.assertIn("needs.changes.outputs.agent != 'false'", condition)
                self.assertNotIn("head.repo", condition)

    def test_python_packaging_retains_portable_and_nix_validation(self):
        config = workflow("agent-ci.yml")
        self.assertEqual(config["defaults"]["run"]["working-directory"], "apps/maple-agent")
        desktop = config["jobs"]["desktop"]
        self.assertIn({"os": "macos-15-intel", "focused": True},
                      desktop["strategy"]["matrix"]["include"])
        steps = desktop["steps"]
        commands = "\n".join(step.get("run", "") for step in steps)
        for command in ("just ci", "just python-test", "scripts/test-python-worker.py",
                        "-m unittest discover -s scripts/tests",
                        "cargo test -p maple-code-mode --locked",
                        "scripts/macos-debug-app.sh", "just code-mode-smoke",
                        "scripts/check-python-package.py --smoke target/debug/code-mode-smoke.exe",
                        "scripts/check-python-package.py --smoke target/debug/code-mode-smoke"):
            self.assertIn(command, commands)
        archive = next(step for step in steps if step.get("name") == "Stage complete Linux CI archive")
        self.assertIn("scripts/prepare-python.py", archive["run"])
        self.assertIn("--runtime target/release/runtime/python", archive["run"])
        self.assertIn("--output-dir target/ci-dist", archive["run"])
        upload = next(step for step in steps if "upload-artifact@" in step.get("uses", ""))
        self.assertEqual(upload["with"]["path"], "apps/maple-agent/target/ci-dist/*")
        nix = config["jobs"]["nix-python"]
        self.assertEqual(nix["runs-on"], "ubuntu-24.04-arm")
        commands = "\n".join(step.get("run", "") for step in nix["steps"])
        for command in ("nix build --no-update-lock-file .#default",
                        "export MAPLE_CODE_MODE_RUNTIME_MANIFEST=", "just python-test",
                        "cargo test -p maple-code-mode --locked", "just code-mode-smoke",
                        "nix path-info --recursive ./result", "interpreter.parents[1]"):
            self.assertIn(command, commands)

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
