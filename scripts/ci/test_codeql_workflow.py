"""Lock CodeQL advanced setup to master and weekly scans, not pull requests."""

import functools
import json
from pathlib import Path
import re
import subprocess
import unittest


ROOT = Path(__file__).resolve().parents[2]
SHA = re.compile(r"^[0-9a-f]{40}$")


@functools.cache
def workflow():
    result = subprocess.run(
        ["yq", "-o=json", ".", str(ROOT / ".github/workflows/codeql.yml")],
        check=True, capture_output=True, text=True,
    )
    return json.loads(result.stdout)


class CodeQLWorkflowTests(unittest.TestCase):
    def test_scans_master_and_weekly_not_pull_requests(self):
        config = workflow()
        self.assertEqual(set(config["on"]), {"push", "schedule", "workflow_dispatch"})
        self.assertEqual(config["on"]["push"]["branches"], ["master"])
        self.assertTrue(config["on"]["schedule"][0]["cron"])
        self.assertEqual(config["permissions"], {"contents": "read"})

    def test_analyzes_default_setup_languages_without_a_build(self):
        job = workflow()["jobs"]["analyze"]
        self.assertGreaterEqual(job["timeout-minutes"], 90)
        self.assertEqual(job["permissions"]["security-events"], "write")
        languages = [row["language"] for row in job["strategy"]["matrix"]["include"]]
        self.assertEqual(
            languages,
            ["actions", "javascript-typescript", "python", "rust"],
        )
        self.assertTrue(all(row["build-mode"] == "none" for row in job["strategy"]["matrix"]["include"]))

    def test_actions_are_pinned_and_checkout_is_unprivileged(self):
        steps = workflow()["jobs"]["analyze"]["steps"]
        self.assertEqual(len(steps), 3)
        for step in steps:
            action = step["uses"]
            self.assertRegex(action, r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+(?:/[A-Za-z0-9_.-]+)?@[0-9a-f]{40}$")
            self.assertRegex(action.rsplit("@", 1)[1], SHA)
        checkout = steps[0]
        self.assertTrue(checkout["uses"].startswith("actions/checkout@"))
        self.assertIs(checkout["with"]["persist-credentials"], False)


if __name__ == "__main__":
    unittest.main()
