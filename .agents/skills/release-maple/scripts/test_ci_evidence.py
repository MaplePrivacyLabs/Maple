"""Release gating regressions: never confuse workflow success with build proof."""

import subprocess
import tempfile
import unittest
from pathlib import Path

from ci_evidence import EvidenceError, eligible_runs, jobs_executed, select_evidence, trees_compatible, unrelated_path


HEAD = "a" * 40
ANCESTOR = "b" * 40
REPO = "MaplePrivacyLabs/Maple"


def run(sha=HEAD, **changes):
    value = {"id": 20, "head_sha": sha, "status": "completed", "conclusion": "success",
             "event": "push", "head_branch": "master", "head_repository": {"full_name": REPO},
             "path": ".github/workflows/frontend-tests.yml", "created_at": "2026-10-05T08:00:00Z",
             "html_url": "https://github.com/MaplePrivacyLabs/Maple/actions/runs/20"}
    value.update(changes)
    return value


def job(name="test-frontend", conclusion="success", status="completed"):
    return {"name": name, "conclusion": conclusion, "status": status}


class EvidenceTests(unittest.TestCase):
    def select(self, runs, jobs=None, compatible=lambda sha: True, **kwargs):
        return select_evidence(runs, HEAD, ("test-frontend",),
                               lambda run_id: (jobs or {20: [job()]})[run_id], compatible, **kwargs)

    def test_exact_head_success(self):
        self.assertFalse(self.select([run()])["reused"])

    def test_unchanged_ancestor_records_reused_sha(self):
        result = self.select([run(ANCESTOR)])
        self.assertTrue(result["reused"])
        self.assertEqual(result["head_sha"], ANCESTOR)

    def test_green_skipped_build_is_not_evidence(self):
        with self.assertRaises(EvidenceError):
            self.select([run()], {20: [job(conclusion="skipped")]})

    def test_skipped_current_build_finds_executed_ancestor(self):
        result = self.select([run(), run(ANCESTOR, id=19)],
                             {20: [job(conclusion="skipped")], 19: [job()]})
        self.assertEqual(result["run_id"], 19)

    def test_changed_inputs_or_non_ancestor_fail(self):
        with self.assertRaises(EvidenceError):
            self.select([run(ANCESTOR)], compatible=lambda sha: False)

    def test_newer_failure_pending_or_cancelled_cannot_fall_back(self):
        for changes in ({"conclusion": "failure"}, {"status": "in_progress", "conclusion": None},
                        {"conclusion": "cancelled"}):
            with self.subTest(changes=changes), self.assertRaises(EvidenceError):
                self.select([run(**changes), run(ANCESTOR, id=19)])

    def test_failed_or_pending_job_in_green_workflow_is_rejected(self):
        for item in (job(conclusion="failure"), job(status="in_progress", conclusion=None)):
            with self.subTest(item=item), self.assertRaises(EvidenceError):
                self.select([run()], {20: [item]})

    def test_missing_required_job_is_rejected(self):
        with self.assertRaises(EvidenceError):
            self.select([run()], {20: [job("unrelated")]})

    def test_partial_platform_build_is_rejected(self):
        with self.assertRaises(EvidenceError):
            jobs_executed([job("macos"), job("windows", "skipped")], ("macos", "windows"))

    def test_codeql_cannot_reuse_ancestor(self):
        with self.assertRaises(EvidenceError):
            self.select([run(ANCESTOR)], exact_only=True)

    def test_codeql_requires_analysis(self):
        with self.assertRaises(EvidenceError):
            jobs_executed([job("changes")], ())
        self.assertTrue(jobs_executed([job("Analyze (rust)")], ()))

    def test_invalid_commit_is_rejected(self):
        with self.assertRaises(EvidenceError):
            self.select([run("not-a-sha")])

    def test_empty_history_is_rejected(self):
        with self.assertRaises(EvidenceError):
            self.select([])

    def test_only_canonical_master_push_can_supply_evidence(self):
        wrong = [{"event": "pull_request"}, {"event": "workflow_dispatch"},
                 {"head_branch": "feature"}, {"head_repository": {"full_name": "attacker/Maple"}},
                 {"path": ".github/workflows/other.yml"}]
        runs = [run(**changes) for changes in wrong] + [run()]
        self.assertEqual(eligible_runs(runs, REPO, "frontend-tests.yml"), [run()])

    def test_newest_push_orders_before_old_green(self):
        old = run(ANCESTOR, id=19, created_at="2026-10-04T08:00:00Z")
        self.assertEqual(eligible_runs([old, run()], REPO, "frontend-tests.yml"), [run(), old])

    def test_only_known_independent_changes_can_reuse(self):
        for path in ("apps/maple-agent/scripts/macos-release-app.sh", "docs/repository-workflows.md",
                     ".agents/skills/release-maple/scripts/preflight.sh", "AGENTS.md",
                     "scripts/ci/test-release-gates.sh"):
            self.assertTrue(unrelated_path(path), path)
        for path in ("apps/maple-research/frontend/src/app.tsx", "proxy/src/lib.rs", "sdk/src/lib/pcr.ts",
                     "crates/shared/src/lib.rs", "flake.nix", "flake.lock", "scripts/ci/frontend.sh",
                     ".github/workflows/frontend-tests.yml", "apps/maple-auth/src/main.tsx", "new-input"):
            self.assertFalse(unrelated_path(path), path)

    def test_remote_git_trees_must_be_complete(self):
        with self.assertRaises(EvidenceError):
            trees_compatible({"truncated": True, "tree": []}, {"truncated": False, "tree": []})

    def test_remote_git_tree_changes_include_modes_and_shared_inputs(self):
        before = {"truncated": False, "tree": [
            {"path": "shared/input", "sha": HEAD, "mode": "100644", "type": "blob"}]}
        after = {"truncated": False, "tree": [
            {"path": "shared/input", "sha": HEAD, "mode": "100755", "type": "blob"}]}
        self.assertFalse(trees_compatible(before, after))
        self.assertTrue(trees_compatible(before, before))

    def test_local_tree_diff_covers_more_than_github_file_limit_and_renames(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            def git(*args):
                return subprocess.check_output(["git", "-C", directory, *args], text=True)
            git("init", "-q"); git("config", "user.email", "test@example.invalid")
            git("config", "user.name", "Release gate test")
            (root / "apps/maple-agent").mkdir(parents=True)
            for index in range(301):
                (root / f"apps/maple-agent/{index}").write_text("before")
            git("add", "."); git("commit", "-qm", "before")
            before = git("rev-parse", "HEAD").strip()
            for index in range(301):
                (root / f"apps/maple-agent/{index}").write_text("after")
            (root / "shared").mkdir()
            (root / "apps/maple-agent/0").rename(root / "shared/input")
            git("add", "."); git("commit", "-qm", "after")
            paths = git("diff", "--name-only", "--no-renames", "-z", before, "HEAD").split("\0")
            self.assertGreater(len([path for path in paths if path]), 300)
            self.assertFalse(all(unrelated_path(path) for path in paths if path))


if __name__ == "__main__":
    unittest.main()
