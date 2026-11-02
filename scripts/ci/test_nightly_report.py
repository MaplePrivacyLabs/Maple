"""The nightly report keeps exactly one ci-nightly issue in step with the checks."""

import contextlib
import io
import json
import os
import subprocess
import unittest
from unittest import mock

import nightly_report
from test_contracts import expression_value
from test_workflow_policy import WORKFLOWS, load

DAY = "2026-10-26"
GREEN = {"status": "completed", "conclusion": "success", "created_at": DAY + "T04:17:00Z", "html_url": "https://ci/1"}
RUNNING = dict(GREEN, status="in_progress", conclusion=None)


class Clock:
    """time.monotonic and time.sleep, without the waiting."""

    def __init__(self):
        self.now, self.sleeps = 0.0, []

    def monotonic(self):
        return self.now

    def sleep(self, seconds):
        self.sleeps.append(seconds)
        self.now += seconds


def today():
    return nightly_report.datetime.now(nightly_report.timezone.utc).strftime("%Y-%m-%d")


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

    def scheduled(self, responses, wait_seconds):
        """scheduled_ci_run against successive API responses (an exception is raised), and its sleeps."""
        responses, clock = list(responses), Clock()

        def gh(path, method="GET", payload=None):
            self.assertIn("/actions/workflows/ci.yml/runs?event=schedule&branch=master", path)
            response = responses.pop(0) if len(responses) > 1 else responses[0]
            if isinstance(response, Exception):
                raise response
            return {"workflow_runs": [response] if response else []}

        with mock.patch.object(nightly_report, "gh", gh), mock.patch.object(nightly_report, "time", clock):
            return nightly_report.scheduled_ci_run("o/r", DAY, wait_seconds), clock.sleeps

    def test_waits_for_todays_scheduled_ci_run_to_finish_within_its_bound(self):
        # GitHub starts scheduled runs hours late: the report may begin before ci.yml's run
        # exists (the newest is yesterday's) or while it still runs, as on 2026-10-30.
        yesterday = dict(GREEN, created_at="2026-10-25T04:17:00Z")
        self.assertEqual(self.scheduled([yesterday, None, RUNNING, RUNNING, GREEN], 7200),
                         (GREEN, [60, 60, 60, 60]))
        # A red or cancelled run has finished too: it ends the wait and is reported as it is.
        for conclusion in ("failure", "cancelled"):
            finished = dict(GREEN, conclusion=conclusion)
            self.assertEqual(self.scheduled([finished], 7200), (finished, []))
            self.assertEqual(self.scheduled([RUNNING, finished], 7200), (finished, [60]))
        # A transient API error or a hung call while waiting is retried.
        failed = subprocess.CalledProcessError(1, "gh")
        self.assertEqual(self.scheduled([failed, GREEN], 7200), (GREEN, [60]))
        hung = subprocess.TimeoutExpired("gh", nightly_report.GH_TIMEOUT_SECONDS)
        self.assertEqual(self.scheduled([hung, GREEN], 7200), (GREEN, [60]))
        # The wait is bounded: after it, the report names the unfinished or missing run.
        self.assertEqual(self.scheduled([RUNNING], 150), (RUNNING, [60, 60, 30]))
        self.assertEqual(self.scheduled([yesterday], 150), (yesterday, [60, 60, 30]))
        with self.assertRaises(subprocess.CalledProcessError):
            self.scheduled([failed], 150)
        # Without a wait it reads the run once.
        self.assertEqual(self.scheduled([RUNNING], 0), (RUNNING, []))

    def test_every_api_call_is_bounded(self):
        # A hung call fails and, while waiting, is retried, instead of holding the job.
        def run(args, **kwargs):
            self.assertGreater(kwargs.get("timeout") or 0, 0)
            return subprocess.CompletedProcess(args, 0, stdout='{"ok": true}')

        with mock.patch.object(nightly_report.subprocess, "run", run):
            self.assertEqual(nightly_report.gh("repos/o/r"), {"ok": True})
            self.assertEqual(nightly_report.gh("repos/o/r/issues", "POST", {"body": "x"}), {"ok": True})

    def run_main(self, dry_run, open_issues, needs, runs=None, wait=""):
        calls, runs = [], list(runs or [dict(GREEN, created_at=today() + "T04:17:00Z")])

        def gh(path, method="GET", payload=None):
            calls.append((method, path, payload))
            if "/actions/workflows/ci.yml/runs" in path:
                return {"workflow_runs": [runs.pop(0) if len(runs) > 1 else runs[0]]}
            return [{"number": number} for number in open_issues] + [{"number": 1, "pull_request": {}}]

        env = {"GITHUB_REPOSITORY": "o/r", "GITHUB_RUN_ID": "9", "NEEDS": json.dumps(needs), "DRY_RUN": dry_run,
               "CI_WAIT_MINUTES": wait}
        with mock.patch.dict(os.environ, env), mock.patch.object(nightly_report, "gh", gh), \
                mock.patch.object(nightly_report, "time", Clock()), contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(nightly_report.main(), 0)
        return [(method, path) for method, path, _ in calls if method != "GET"]

    def test_writes_only_outside_a_dry_run_and_closes_on_green(self):
        failing = {"agent-provider": {"result": "failure"}}
        self.assertEqual(self.run_main("true", [], failing), [])
        self.assertEqual(self.run_main("false", [], failing), [("POST", "repos/o/r/issues")])
        self.assertEqual(self.run_main("false", [5], failing), [("POST", "repos/o/r/issues/5/comments")])
        self.assertEqual(self.run_main("false", [5], {}),
                         [("POST", "repos/o/r/issues/5/comments"), ("PATCH", "repos/o/r/issues/5")])

    def test_a_green_ci_run_that_finishes_while_the_report_waits_closes_the_issue(self):
        # 2026-10-30: the report ran at 11:49:49 and ci.yml's run finished at 11:52:23, three
        # polls later. CI_WAIT_MINUTES is minutes: read as seconds, 120 would end the wait first.
        running, green = (dict(run, created_at=today() + "T10:54:36Z") for run in (RUNNING, GREEN))
        closed = [("POST", "repos/o/r/issues/5/comments"), ("PATCH", "repos/o/r/issues/5")]
        self.assertEqual(self.run_main("false", [5], {}, [running, running, running, green], wait="120"), closed)
        # Without the wait, the same night reported "still in_progress" and kept the issue open.
        self.assertEqual(self.run_main("false", [5], {}, [running, green]), [("POST", "repos/o/r/issues/5/comments")])

    def test_the_report_job_outlasts_its_wait(self):
        # nightly.yml's Report job must still have time to write the report after the longest
        # wait (and a last slow poll), or a timeout leaves the night unreported.
        job = load(WORKFLOWS / "nightly.yml")["jobs"]["report"]
        step = next(step for step in job["steps"] if "nightly_report.py" in step.get("run", ""))
        wait = step["env"]["CI_WAIT_MINUTES"].strip().removeprefix("${{").removesuffix("}}")
        wait = wait.replace("&&", " and ").replace("||", " or ")
        waits = {event: float(expression_value(wait, {"github.event_name": event}))
                 for event in ("schedule", "workflow_dispatch")}
        self.assertGreater(waits["schedule"], 0)
        self.assertEqual(waits["workflow_dispatch"], 0)  # a dry run reports at once
        for event, minutes in waits.items():
            with self.subTest(event=event):
                self.assertGreaterEqual(job["timeout-minutes"] - minutes, 5)


if __name__ == "__main__":
    unittest.main()
