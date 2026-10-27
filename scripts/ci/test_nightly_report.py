"""The nightly report keeps exactly one ci-nightly issue in step with the checks."""

import contextlib
import io
import json
import os
import unittest
from unittest import mock

import nightly_report

DAY = "2026-10-26"
GREEN = {"status": "completed", "conclusion": "success", "created_at": DAY + "T04:17:00Z", "html_url": "https://ci/1"}


class NightlyReportTests(unittest.TestCase):
    def test_failures_list_failed_jobs_and_a_red_stale_running_or_missing_scheduled_ci_run(self):
        needs = {"agent-provider": {"result": "failure"},
                 "bun-audit": {"result": "cancelled"}}
        self.assertEqual(nightly_report.failures(needs, GREEN, DAY),
                         ["- `agent-provider`: failure", "- `bun-audit`: cancelled"])
        red = dict(GREEN, conclusion="failure")
        self.assertEqual(nightly_report.failures({}, red, DAY),
                         ["- ci.yml scheduled run of 2026-10-26: failure (https://ci/1)"])
        running = dict(GREEN, status="in_progress", conclusion=None)
        self.assertEqual(nightly_report.failures({}, running, DAY),
                         ["- ci.yml scheduled run of 2026-10-26: still in_progress (https://ci/1)"])
        # Yesterday's green run says nothing about tonight's.
        for stale in (GREEN, None):
            self.assertEqual(nightly_report.failures({}, stale, "2026-10-27"),
                             ["- ci.yml: no scheduled run on master on 2026-10-27"])
        # An active quarantine is listed every night until it is lifted.
        self.assertEqual(nightly_report.failures({}, GREEN, DAY, " os-pcr,agent-test "),
                         ["- `agent-test` is quarantined (`CI_QUARANTINE`): its failures do not fail `ci-ok`",
                          "- `os-pcr` is quarantined (`CI_QUARANTINE`): its failures do not fail `ci-ok`"])

    def test_one_issue_is_created_commented_or_closed(self):
        self.assertEqual(nightly_report.decide(["- x"], None, "u")[0], "create")
        self.assertEqual(nightly_report.decide(["- x"], 7, "u")[0], "comment")
        self.assertEqual(nightly_report.decide([], 7, "u")[0], "close")
        self.assertIsNone(nightly_report.decide([], None, "u"))
        # A green night with an active quarantine keeps the issue open, without calling it a failure.
        quarantine = nightly_report.failures({}, GREEN, DAY, "os-pcr")
        self.assertTrue(nightly_report.decide(quarantine, 7, "u")[1].startswith("Nightly checks passed"))
        self.assertTrue(nightly_report.decide(quarantine + ["- x"], 7, "u")[1].startswith("Nightly checks failed"))

    def run_main(self, dry_run, open_issues, needs):
        calls = []

        def gh(path, method="GET", payload=None):
            calls.append((method, path, payload))
            if "/actions/workflows/ci.yml/runs" in path:
                return {"workflow_runs": [dict(GREEN, created_at=nightly_report.datetime.now(
                    nightly_report.timezone.utc).strftime("%Y-%m-%dT04:17:00Z"))]}
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
