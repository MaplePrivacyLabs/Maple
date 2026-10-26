"""Superseded PR checks may cancel; independent source runs must survive."""

import functools
import json
from pathlib import Path
import re
import subprocess
import unittest


ROOT = Path(__file__).resolve().parents[2]
WORKFLOWS = (
    "sdk-rust.yml", "sdk-typescript.yml", "proxy-rust.yml",
    "proxy-container.yml", "frontend-tests.yml", "rust-tests.yml",
    "repository-checks.yml",
)


@functools.cache
def workflow(name):
    result = subprocess.run(
        ["yq", "-o=json", ".", str(ROOT / ".github/workflows" / name)],
        check=True, capture_output=True, text=True,
    )
    return json.loads(result.stdout)


def concurrency_for(config, event, run_id, pr_number=None):
    """Interpret only the checked-in group grammar, never arbitrary expressions."""
    group = config["concurrency"]["group"]
    choice = re.fullmatch(
        r"\$\{\{ github\.workflow \}\}-\$\{\{ github\.event_name == '([^']+)' "
        r"&& format\('([^']+)', github\.event\.pull_request\.number\) "
        r"\|\| format\('([^']+)', github\.run_id\) \}\}", group,
    )
    cancel = re.fullmatch(
        r"\$\{\{ github\.event_name == '([^']+)' \}\}",
        config["concurrency"]["cancel-in-progress"],
    )
    if choice is None or cancel is None:
        raise AssertionError("unsupported concurrency expression")
    suffix = choice[2].format(pr_number) if event == choice[1] else choice[3].format(run_id)
    return f"{config['name']}-{suffix}".casefold(), event == cancel[1]


class PullRequestConcurrencyTests(unittest.TestCase):
    def test_new_commits_replace_only_the_same_pull_request(self):
        for name in WORKFLOWS:
            with self.subTest(workflow=name):
                config = workflow(name)
                # A new source commit starts a new run for the same PR.
                first = concurrency_for(config, "pull_request", 100, 42)
                next_commit = concurrency_for(config, "pull_request", 101, 42)
                another_pr = concurrency_for(config, "pull_request", 102, 43)
                self.assertEqual(first, next_commit)
                self.assertTrue(first[1])
                self.assertNotEqual(first[0], another_pr[0])

    def test_workflows_do_not_cancel_each_others_checks(self):
        groups = [concurrency_for(workflow(name), "pull_request", 100, 42)[0]
                  for name in WORKFLOWS]
        self.assertEqual(len(groups), len(set(groups)))

    def test_master_manual_and_other_events_keep_independent_runs(self):
        for name in WORKFLOWS:
            for event in ("push", "workflow_dispatch", "schedule"):
                with self.subTest(workflow=name, event=event):
                    config = workflow(name)
                    first = concurrency_for(config, event, 42)
                    next_run = concurrency_for(config, event, 43)
                    pr = concurrency_for(config, "pull_request", 100, 42)
                    self.assertFalse(first[1])
                    self.assertFalse(next_run[1])
                    self.assertNotEqual(first[0], next_run[0])
                    self.assertNotEqual(first[0], pr[0])

    def test_covered_workflows_remain_unprivileged_source_checks(self):
        for name in WORKFLOWS:
            with self.subTest(workflow=name):
                config = workflow(name)
                self.assertEqual(set(config["on"]), {"push", "pull_request"})
                self.assertEqual(config["permissions"], {"contents": "read"})
                for job in config["jobs"].values():
                    self.assertNotIn("uses", job, "Reusable workflows need separate review")
                    self.assertNotIn("environment", job)
                    self.assertIn(job.get("permissions"), (None, {"contents": "read"}))
                    self.assertNotIn("secrets.", json.dumps(job))
                if name == "proxy-container.yml":
                    build = next(step for step in config["jobs"]["proxy-container"]["steps"]
                                 if step.get("uses", "").startswith("docker/build-push-action@"))
                    self.assertIs(build["with"]["push"], False)


if __name__ == "__main__":
    unittest.main()
