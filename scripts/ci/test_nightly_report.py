"""The nightly report keeps exactly one ci-nightly issue in step with the checks."""

import contextlib
import io
import json
import os
import unittest
from unittest import mock

import nightly_report

GREEN = {"conclusion": "success", "created_at": "2026-10-26T04:17:00Z", "html_url": "https://ci/1"}


class NightlyReportTests(unittest.TestCase):
    def test_failures_list_failed_jobs_and_a_red_or_missing_scheduled_ci_run(self):
        needs = {"store-bench": {"result": "success"}, "agent-provider": {"result": "failure"},
                 "advisories": {"result": "cancelled"}}
        self.assertEqual(nightly_report.failures(needs, GREEN),
                         ["- `advisories`: cancelled", "- `agent-provider`: failure"])
        red = dict(GREEN, conclusion="failure")
        self.assertEqual(nightly_report.failures({}, red),
                         ["- ci.yml scheduled run of 2026-10-26: failure (https://ci/1)"])
        self.assertEqual(len(nightly_report.failures({}, None)), 1)

    def test_one_issue_is_created_commented_or_closed(self):
        self.assertEqual(nightly_report.decide(["- x"], None, "u")[0], "create")
        self.assertEqual(nightly_report.decide(["- x"], 7, "u")[0], "comment")
        self.assertEqual(nightly_report.decide([], 7, "u")[0], "close")
        self.assertIsNone(nightly_report.decide([], None, "u"))

    def run_main(self, dry_run, open_issues, needs):
        calls = []

        def gh(path, method="GET", payload=None):
            calls.append((method, path, payload))
            if "/actions/workflows/ci.yml/runs" in path:
                return {"workflow_runs": [GREEN]}
            return [{"number": number} for number in open_issues] + [{"number": 1, "pull_request": {}}]

        env = {"GITHUB_REPOSITORY": "o/r", "GITHUB_RUN_ID": "9", "NEEDS": json.dumps(needs), "DRY_RUN": dry_run}
        with mock.patch.dict(os.environ, env), mock.patch.object(nightly_report, "gh", gh), \
                contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(nightly_report.main(), 0)
        return [(method, path) for method, path, _ in calls if method != "GET"]

    def test_writes_only_outside_a_dry_run_and_closes_on_green(self):
        failing = {"agent-provider": {"result": "failure"}}
        self.assertEqual(self.run_main("true", [], failing), [])
        self.assertEqual(self.run_main("false", [], failing), [("POST", "repos/o/r/issues")])
        self.assertEqual(self.run_main("false", [5], failing), [("POST", "repos/o/r/issues/5/comments")])
        self.assertEqual(self.run_main("false", [5], {}),
                         [("POST", "repos/o/r/issues/5/comments"), ("PATCH", "repos/o/r/issues/5")])


if __name__ == "__main__":
    unittest.main()
