"""Run the legacy CI signer against real producer output and public signatures.

The node stand-in replays an existing public signature, so these tests never
need a private key. The real Just recipe and signature verifier still run.
Run this signing harness with the network disabled.
"""

import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

from test_opensecret_eif_scripts import ROOT, release_build_candidate
from test_opensecret_release_statement import git


class SigningScenario:
    """A real repository, producer and verifier with public signature replay."""

    def __init__(self, root, mode):
        self.root = root
        self.mode = mode
        self.repo = root / "repo"
        self.component = self.repo / "services/opensecret"
        self.snapshot = self.component / f"pcr{mode.title()}.json"
        self.history_path = self.component / f"pcr{mode.title()}History.json"
        self.summary = root / "summary.md"
        self.calls = root / "signer-calls"
        self.install_sources()
        self.entries = json.loads((ROOT / "services/opensecret" / self.history_path.name).read_bytes())[:3]
        self.git("init", "-q", "--initial-branch=master")
        self.git("config", "diff.renames", "true")
        self.commit("fixture build")
        self.binaries = root / "bin"
        self.binaries.mkdir()
        signatures = {entry["PCR0"]: entry["signature"] for entry in self.entries}
        node = self.binaries / "node"
        node.write_text(f"""#!{sys.executable}
import sys
from pathlib import Path
with Path({str(self.calls)!r}).open('a') as output:
    output.write('sign\\n')
print({signatures!r}[sys.argv[3]])
""")
        node.chmod(0o755)
        self.env = dict(os.environ, PATH=f"{self.binaries}:{os.environ['PATH']}",
                        SIGNING_PRIVATE_KEY="unused-public-signature-replay",
                        GITHUB_STEP_SUMMARY=str(self.summary), PYTHONDONTWRITEBYTECODE="1")

    def install_sources(self):
        for relative in ("services/opensecret/justfile", "services/opensecret/scripts/ci_sign_pcr.sh",
                         "services/opensecret/scripts/pcr_compatibility.py",
                         "scripts/ci/opensecret_change_detection.py", "scripts/ci/publish_opensecret_approval.sh"):
            target = self.repo / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(ROOT / relative, target)
        for environment in ("Dev", "Prod"):
            entries = json.loads((ROOT / f"services/opensecret/pcr{environment}History.json").read_bytes())
            (self.component / f"pcr{environment}History.json").write_text(json.dumps(entries[:1]) + "\n")
            (self.component / f"pcr{environment}.json").write_bytes(self.measurements(entries[0]))
        source = self.component / "src/extra.rs"
        source.parent.mkdir()
        source.write_text("// fixture input\n")

    @staticmethod
    def measurements(entry):
        pcr = {field: entry[field] for field in ("PCR0", "PCR1", "PCR2")}
        return (json.dumps({"HashAlgorithm": "Sha384 { ... }", **pcr}) + "\n").encode()

    def git(self, *arguments):
        return git(*arguments, cwd=self.repo)

    def commit(self, message):
        self.git("add", ".")
        self.git("commit", "-qm", message)
        return self.git("rev-parse", "HEAD")

    def change(self, path):
        target = self.repo / path
        target.parent.mkdir(parents=True, exist_ok=True)
        with target.open("a") as output:
            output.write("\n")
        return self.commit("later fixture")

    def change_eif_input(self):
        return self.change("services/opensecret/src/later.rs")

    def change_docs_and_approvals(self):
        for path in ("docs/release.md", "services/opensecret/docs/release.md",
                     "services/opensecret/pcrDevHistory.json", "services/opensecret/pcrProd.json"):
            self.change(path)

    def merge(self, path):
        self.git("checkout", "-qb", "fixture-branch")
        self.change(path)
        self.git("checkout", "-q", "master")
        self.git("merge", "--no-ff", "--no-commit", "fixture-branch")

    def merge_eif_input(self):
        self.merge("services/opensecret/src/merged.rs")
        self.commit("merge fixture")

    def merge_docs(self):
        self.merge("docs/merged.md")
        self.commit("merge fixture")

    def merge_resolution_eif_input(self):
        self.merge("docs/merged.md")
        self.change("services/opensecret/src/resolution.rs")

    def revert_eif_input(self):
        self.change_eif_input()
        self.git("revert", "--no-edit", "HEAD")

    def rename_eif_input_to_docs(self):
        (self.component / "docs").mkdir()
        self.git("mv", "services/opensecret/src/extra.rs", "services/opensecret/docs/extra.rs")
        self.commit("move input into docs")

    def build(self, entry_index=1):
        work = self.root / f"build-{entry_index}"
        work.mkdir()
        return release_build_candidate(work, mode=self.mode, source_sha=self.git("rev-parse", "HEAD"),
                                       image=b"fixture EIF", pcr=self.measurements(self.entries[entry_index]))

    def sign(self, candidate):
        return subprocess.run(["bash", str(self.component / "scripts/ci_sign_pcr.sh"), self.mode, str(candidate)],
                              cwd=self.repo, env=self.env, capture_output=True, text=True)

    def history(self):
        return json.loads(self.history_path.read_bytes())

    def signing_calls(self):
        return len(self.calls.read_text().splitlines()) if self.calls.exists() else 0

    def start_origin(self):
        self.git("init", "-q", "--bare", str(self.root / "origin.git"))
        self.git("remote", "add", "origin", str(self.root / "origin.git"))
        self.git("push", "-q", "origin", "master")

    def publish(self, candidate):
        handoff = json.loads((candidate / "handoff.json").read_bytes())
        env = dict(self.env, GITHUB_TOKEN="fixture-token", GITHUB_REPOSITORY="fixture/repo",
                   EXPECTED_SOURCE_SHA=handoff["source_sha"])
        return subprocess.run(["bash", str(self.repo / "scripts/ci/publish_opensecret_approval.sh"),
                               self.mode, str(candidate)], cwd=self.repo, env=env, capture_output=True, text=True)


class LegacySigningTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        tools = ("bash", "just", "jq", "git", "dirname", "mktemp", "rm", "ls", "mkdir", "cp",
                 "chmod", "sha256sum", "cut", "cmp", "base64", "tr", "sleep")
        missing = [tool for tool in tools if shutil.which(tool) is None]
        has_crypto = importlib.util.find_spec("cryptography") is not None
        if not has_crypto:
            missing.append("Python cryptography")
        if missing:
            message = "Missing signing test dependencies: " + ", ".join(missing)
            if os.environ.get("CI"):
                raise RuntimeError(message)
            raise unittest.SkipTest(message)

    def scenario(self, mode):
        temporary = self.enterContext(tempfile.TemporaryDirectory())
        return SigningScenario(Path(temporary), mode)

    def assert_signed(self, scenario, candidate):
        result = scenario.sign(candidate)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        return result

    def assert_history(self, scenario, *indices):
        entries = scenario.history()
        self.assertEqual(len(entries), len(indices))
        self.assertEqual(entries[0], scenario.entries[0])
        for entry, index in zip(entries, indices):
            expected = scenario.entries[index]
            for field in ("PCR0", "PCR1", "PCR2", "signature"):
                self.assertEqual(entry[field], expected[field])

    def test_current_build_updates_snapshot_after_appending_history(self):
        for mode in ("dev", "prod"):
            with self.subTest(mode=mode):
                scenario = self.scenario(mode)
                candidate = scenario.build()
                result = self.assert_signed(scenario, candidate)
                self.assertEqual(scenario.snapshot.read_bytes(), (candidate / "pcr.json").read_bytes())
                self.assert_history(scenario, 0, 1)
                self.assertIn("Current build", result.stdout)
                self.assertIn("Current build", scenario.summary.read_text())

    def test_older_build_appends_history_without_changing_current_snapshot(self):
        for mode in ("dev", "prod"):
            with self.subTest(mode=mode):
                scenario = self.scenario(mode)
                candidate = scenario.build()
                scenario.change_eif_input()
                before = scenario.snapshot.read_bytes()
                result = self.assert_signed(scenario, candidate)
                self.assertEqual(scenario.snapshot.read_bytes(), before)
                self.assert_history(scenario, 0, 1)
                for output in (result.stdout, scenario.summary.read_text()):
                    self.assertIn("Older build", output)
                    self.assertIn("services/opensecret/src/later.rs", output)

    def test_approved_rollback_changes_neither_snapshot_nor_history(self):
        for mode in ("dev", "prod"):
            with self.subTest(mode=mode):
                scenario = self.scenario(mode)
                candidate = scenario.build()
                scenario.history_path.write_text(json.dumps(scenario.entries[:2]) + "\n")
                scenario.commit("existing history approval")
                scenario.change_eif_input()
                before = scenario.snapshot.read_bytes(), scenario.history_path.read_bytes()
                self.assert_signed(scenario, candidate)
                self.assertEqual((scenario.snapshot.read_bytes(), scenario.history_path.read_bytes()), before)
                self.assertEqual(scenario.signing_calls(), 0)

    def test_identical_signing_rerun_preserves_history(self):
        for mode in ("dev", "prod"):
            with self.subTest(mode=mode):
                scenario = self.scenario(mode)
                candidate = scenario.build()
                self.assert_signed(scenario, candidate)
                before = scenario.history_path.read_bytes()
                # A retry must reuse the existing approval without signing or changing its timestamp.
                self.assert_signed(scenario, candidate)
                self.assertEqual(scenario.history_path.read_bytes(), before)
                self.assertEqual(scenario.signing_calls(), 1)

    def test_docs_and_approval_commits_after_build_still_update_current_snapshot(self):
        for mode in ("dev", "prod"):
            with self.subTest(mode=mode):
                scenario = self.scenario(mode)
                candidate = scenario.build()
                scenario.change_docs_and_approvals()
                self.assert_signed(scenario, candidate)
                self.assertEqual(scenario.snapshot.read_bytes(), (candidate / "pcr.json").read_bytes())

    def test_merge_of_eif_input_leaves_current_snapshot_unchanged(self):
        for mode in ("dev", "prod"):
            with self.subTest(mode=mode):
                scenario = self.scenario(mode)
                candidate = scenario.build()
                scenario.merge_eif_input()
                before = scenario.snapshot.read_bytes()
                self.assert_signed(scenario, candidate)
                self.assertEqual(scenario.snapshot.read_bytes(), before)
                self.assertEqual(len(scenario.git("rev-list", "--parents", "-n", "1", "HEAD").split()), 3)

    def test_docs_only_merge_still_updates_current_snapshot(self):
        for mode in ("dev", "prod"):
            with self.subTest(mode=mode):
                scenario = self.scenario(mode)
                candidate = scenario.build()
                scenario.merge_docs()
                self.assert_signed(scenario, candidate)
                self.assertEqual(scenario.snapshot.read_bytes(), (candidate / "pcr.json").read_bytes())
                self.assertEqual(len(scenario.git("rev-list", "--parents", "-n", "1", "HEAD").split()), 3)

    def test_eif_input_introduced_by_merge_resolution_leaves_snapshot_unchanged(self):
        for mode in ("dev", "prod"):
            with self.subTest(mode=mode):
                scenario = self.scenario(mode)
                candidate = scenario.build()
                scenario.merge_resolution_eif_input()
                before = scenario.snapshot.read_bytes()
                self.assert_signed(scenario, candidate)
                self.assertEqual(scenario.snapshot.read_bytes(), before)
                self.assertEqual(len(scenario.git("rev-list", "--parents", "-n", "1", "HEAD").split()), 3)

    def test_reverted_eif_input_still_updates_current_snapshot(self):
        for mode in ("dev", "prod"):
            with self.subTest(mode=mode):
                scenario = self.scenario(mode)
                candidate = scenario.build()
                source = scenario.git("rev-parse", "HEAD")
                scenario.revert_eif_input()
                self.assertEqual(scenario.git("diff", "--name-only", source, "HEAD"), "")
                self.assert_signed(scenario, candidate)
                self.assertEqual(scenario.snapshot.read_bytes(), (candidate / "pcr.json").read_bytes())

    def test_renaming_an_eif_input_into_docs_cannot_restore_an_older_snapshot(self):
        for mode in ("dev", "prod"):
            with self.subTest(mode=mode):
                scenario = self.scenario(mode)
                old = scenario.build()
                scenario.rename_eif_input_to_docs()
                new = scenario.build(2)
                self.assert_signed(scenario, new)
                scenario.commit("current approval")
                self.assert_signed(scenario, old)
                self.assertEqual(scenario.snapshot.read_bytes(), (new / "pcr.json").read_bytes())
                self.assert_history(scenario, 0, 2, 1)

    def test_source_from_a_side_branch_is_refused_without_signing(self):
        for mode in ("dev", "prod"):
            with self.subTest(mode=mode):
                scenario = self.scenario(mode)
                scenario.git("checkout", "-qb", "side")
                scenario.change_eif_input()
                candidate = scenario.build()
                scenario.git("checkout", "-q", "master")
                before = scenario.snapshot.read_bytes(), scenario.history_path.read_bytes()
                result = scenario.sign(candidate)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("outside this checkout's history", result.stderr)
                self.assertEqual((scenario.snapshot.read_bytes(), scenario.history_path.read_bytes()), before)
                self.assertEqual(scenario.signing_calls(), 0)

    def test_pending_approval_branch_is_verified_and_reused_on_rerun(self):
        for mode in ("dev", "prod"):
            with self.subTest(mode=mode):
                scenario = self.scenario(mode)
                candidate = scenario.build()
                checkout = scenario.change_eif_input()
                scenario.start_origin()
                self.assert_signed(scenario, candidate)
                first = scenario.publish(candidate)
                self.assertEqual(first.returncode, 0, first.stderr)
                summary = scenario.summary.read_text()
                self.assertIn("History-only approval", summary)
                self.assertNotIn("OPENSECRET_SOURCE_REF", summary)
                # A fresh runner recreates the proposal; the existing branch must keep its original entry.
                scenario.git("reset", "--hard", checkout)
                self.assert_signed(scenario, candidate)
                second = scenario.publish(candidate)
                self.assertEqual(second.returncode, 0, second.stderr)
                self.assertIn("keeping its existing history", second.stdout)

    def test_current_approval_publisher_reports_the_deployment_handoff(self):
        for mode in ("dev", "prod"):
            with self.subTest(mode=mode):
                scenario = self.scenario(mode)
                candidate = scenario.build()
                scenario.start_origin()
                self.assert_signed(scenario, candidate)
                result = scenario.publish(candidate)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn("OPENSECRET_SOURCE_REF", scenario.summary.read_text())


class SigningToolRequirementsTests(unittest.TestCase):
    def test_missing_tools_fail_in_ci_and_skip_outside_ci(self):
        with tempfile.TemporaryDirectory() as temporary:
            probe = (
                "import sys, unittest; "
                f"sys.path.insert(0, {str(Path(__file__).parent)!r}); "
                "from test_opensecret_legacy_signing import LegacySigningTests; "
                "suite = unittest.defaultTestLoader.loadTestsFromTestCase(LegacySigningTests); "
                "result = unittest.TextTestRunner().run(suite); "
                "raise SystemExit(not result.wasSuccessful())"
            )
            env = dict(os.environ, PATH=temporary, CI="true")
            command = [sys.executable, "-B", "-I", "-c", probe]
            ci = subprocess.run(command, env=env, capture_output=True, text=True)
            self.assertNotEqual(ci.returncode, 0)
            self.assertIn("Missing signing test dependencies", ci.stderr)
            self.assertIn("FAILED", ci.stderr)
            env.pop("CI")
            local = subprocess.run(command, env=env, capture_output=True, text=True)
            self.assertEqual(local.returncode, 0, local.stderr)
            self.assertIn("OK (skipped=1)", local.stderr)
