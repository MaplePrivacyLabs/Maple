"""Master packaging must retain changes until all profiles are verified."""

from contextlib import redirect_stderr, redirect_stdout
import io
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

import agent_master_selection as selector


def run_record(run_id, head, **overrides):
    run = {
        "id": run_id,
        "head_sha": head,
        "repository": {"full_name": selector.REPOSITORY},
        "head_repository": {"full_name": selector.REPOSITORY},
        "head_branch": "master",
        "event": "push",
        "path": selector.WORKFLOW,
        "status": "completed",
        "conclusion": "success",
    }
    run.update(overrides)
    return run


def verified_jobs(**conclusions):
    jobs = [
        {"name": name, "status": "completed",
         "conclusion": conclusions.get(name, "success")}
        for name in sorted(selector.VERIFIERS)
    ]
    return {"total_count": len(jobs), "jobs": jobs}


class MasterSelectionTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.git("init", "--quiet")
        self.base = self.commit("apps/maple-agent/app/src/main.rs", "original\n")

    def git(self, *args):
        return subprocess.run(
            ["git", "-c", "user.name=CI test", "-c", "user.email=ci@example.invalid",
             *args], cwd=self.root, check=True, capture_output=True, text=True,
        ).stdout.strip()

    def commit(self, path, content):
        target = self.root / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(content)
        self.git("add", "--", path)
        self.git("commit", "--quiet", "-m", "fixture")
        return self.git("rev-parse", "HEAD")

    def select(self, head, runs, jobsets=None, current_run=999):
        jobsets = jobsets or {}

        def response(path):
            if "/workflows/" in path:
                return {"workflow_runs": runs}
            run_id = int(path.split("/runs/")[1].split("/")[0])
            return jobsets.get(run_id, verified_jobs())

        with patch.object(selector, "api", side_effect=response) as api:
            result = selector.select_master(head, current_run, self.root)
        return result, api

    def test_failed_or_inflight_change_remains_selected_after_unrelated_push(self):
        changed = self.commit("apps/maple-agent/app/src/main.rs", "new Agent input\n")
        head = self.commit("README.md", "unrelated later push\n")
        for status, conclusion in (("completed", "failure"), ("queued", None),
                                   ("in_progress", None)):
            with self.subTest(status=status):
                (selected, reason), api = self.select(head, [
                    run_record(2, changed, status=status, conclusion=conclusion),
                    run_record(1, self.base),
                ])
                self.assertTrue(selected)
                self.assertIn("master run 1", reason)
                self.assertEqual(api.call_count, 2)

    def test_missed_run_remains_selected_after_unrelated_push(self):
        self.commit("apps/maple-agent/app/src/main.rs", "missed Agent change\n")
        head = self.commit("README.md", "unrelated\n")
        (selected, _), _ = self.select(head, [run_record(1, self.base)])
        self.assertTrue(selected)

    def test_selector_only_success_does_not_advance_baseline(self):
        changed = self.commit("apps/maple-agent/app/src/main.rs", "not packaged\n")
        head = self.commit("README.md", "unrelated\n")
        jobs = verified_jobs(**{name: "skipped" for name in selector.VERIFIERS})
        (selected, reason), _ = self.select(
            head, [run_record(2, changed), run_record(1, self.base)], {2: jobs},
        )
        self.assertTrue(selected)
        self.assertIn("master run 1", reason)

    def test_each_profile_and_platform_must_have_actual_success(self):
        changed = self.commit("apps/maple-agent/app/src/main.rs", "not fully verified\n")
        head = self.commit("README.md", "unrelated\n")
        for verifier in selector.VERIFIERS:
            with self.subTest(verifier=verifier):
                (selected, reason), _ = self.select(
                    head, [run_record(2, changed), run_record(1, self.base)],
                    {2: verified_jobs(**{verifier: "failure"})},
                )
                self.assertTrue(selected)
                self.assertIn("master run 1", reason)

    def test_previous_macos_linux_only_runs_cannot_consume_windows_changes(self):
        changed = self.commit("apps/maple-agent/app/src/main.rs", "Windows packaging change\n")
        head = self.commit("README.md", "unrelated later push\n")
        jobs = verified_jobs()
        jobs["jobs"] = [job for job in jobs["jobs"] if "Windows" not in job["name"]]
        jobs["total_count"] = len(jobs["jobs"])
        (selected, _), _ = self.select(head, [run_record(2, changed)], {2: jobs})
        self.assertTrue(selected)

    def test_partial_latest_attempt_cannot_borrow_older_green_jobs(self):
        changed = self.commit("apps/maple-agent/app/src/main.rs", "partial retry\n")
        head = self.commit("README.md", "unrelated\n")
        jobs = verified_jobs()
        jobs["jobs"] = jobs["jobs"][:1]
        jobs["total_count"] = 1
        (selected, _), api = self.select(
            head, [run_record(2, changed), run_record(1, self.base)], {2: jobs},
        )
        self.assertTrue(selected)
        jobs_calls = [call.args[0] for call in api.call_args_list if "/jobs?" in call.args[0]]
        self.assertEqual(len(jobs_calls), 2)
        self.assertTrue(all("filter=latest&per_page=100" in path for path in jobs_calls))

    def test_current_run_and_untrusted_candidates_cannot_hide_changes(self):
        head = self.commit("apps/maple-agent/app/src/main.rs", "new input\n")
        candidates = (
            run_record(999, head),
            run_record(2, head, repository={"full_name": "other/Maple"}),
            run_record(2, head, head_repository={"full_name": "other/Maple"}),
            run_record(2, head, head_branch="feature"),
            run_record(2, head, event="pull_request"),
            run_record(2, head, path=".github/workflows/other.yml"),
        )
        for candidate in candidates:
            with self.subTest(candidate=candidate):
                (selected, reason), api = self.select(
                    head, [candidate, run_record(1, self.base)],
                )
                self.assertTrue(selected)
                self.assertIn("master run 1", reason)
                self.assertEqual(api.call_count, 2)

    def test_nonancestor_candidate_is_not_a_baseline(self):
        head = self.commit("apps/maple-agent/app/src/main.rs", "new input\n")
        foreign = self.git("commit-tree", "HEAD^{tree}", "-m", "unrelated history")
        (selected, reason), api = self.select(
            head, [run_record(2, foreign), run_record(1, self.base)],
        )
        self.assertTrue(selected)
        self.assertIn("master run 1", reason)
        self.assertEqual(api.call_count, 2)

    def test_trusted_manual_success_can_supply_verified_baseline(self):
        packaged = self.commit("apps/maple-agent/app/src/main.rs", "packaged\n")
        head = self.commit("README.md", "unrelated\n")
        (selected, reason), _ = self.select(
            head, [run_record(2, packaged, event="workflow_dispatch")],
        )
        self.assertFalse(selected)
        self.assertIn("master run 2", reason)

    def test_unknown_shared_and_selector_inputs_select_packaging(self):
        for path in (
            "new-build-config.toml", "flake.lock", "proxy/src/proxy.rs",
            "sdk/rust/src/client.rs", ".github/workflows/agent-desktop-build.yml",
            "scripts/ci/agent_master_selection.py",
        ):
            with self.subTest(path=path):
                self.git("reset", "--hard", self.base)
                self.git("clean", "-fd")
                head = self.commit(path, "changed\n")
                (selected, _), _ = self.select(head, [run_record(1, self.base)])
                self.assertTrue(selected)

    def test_net_revert_to_verified_inputs_skips_packaging(self):
        self.commit("apps/maple-agent/app/src/main.rs", "reverted change\n")
        self.commit("apps/maple-agent/app/src/main.rs", "original\n")
        head = self.commit("README.md", "unrelated\n")
        (selected, _), _ = self.select(head, [run_record(1, self.base)])
        self.assertFalse(selected)

    def test_no_verified_history_selects_packaging(self):
        (selected, _), _ = self.select(self.base, [])
        self.assertTrue(selected)

    def test_incomplete_job_page_is_not_proof(self):
        jobs = verified_jobs()
        jobs["total_count"] += 1
        with self.assertRaises(ValueError):
            self.select(self.base, [run_record(1, self.base)], {1: jobs})


class MainFallbackTests(unittest.TestCase):
    def invoke(self, event="push", response=None, error=None):
        stdout, stderr = io.StringIO(), io.StringIO()
        environment = {
            "GITHUB_EVENT_NAME": event,
            "GITHUB_REPOSITORY": selector.REPOSITORY,
            "GITHUB_REF": "refs/heads/master",
            "GITHUB_SHA": "a" * 40,
            "GITHUB_RUN_ID": "999",
        }
        with patch.dict(os.environ, environment), patch.object(
            selector, "api", return_value=response, side_effect=error,
        ) as api, patch("sys.argv", ["agent_master_selection.py"]), redirect_stdout(
            stdout,
        ), redirect_stderr(stderr):
            self.assertEqual(selector.main(), 0)
        self.assertEqual(stdout.getvalue(), "agent=true\n")
        self.assertIn("building", stderr.getvalue())
        return api

    def test_missing_history_and_lookup_errors_build(self):
        for response, error in (
            ({"workflow_runs": []}, None),
            ({}, None),
            (None, OSError("unavailable")),
            (None, subprocess.TimeoutExpired("gh", 15)),
        ):
            with self.subTest(response=response, error=error):
                self.invoke(response=response, error=error)

    def test_manual_force_build_avoids_api(self):
        api = self.invoke(event="workflow_dispatch", error=AssertionError("must not query"))
        api.assert_not_called()


if __name__ == "__main__":
    unittest.main()
