"""What proxy-publish.yml's `crate` job does in each registry and source state.

The job only starts the protected crates.io publisher. These tests run its
script with stand-ins for curl, git and gh, so they fail if a case stops
failing closed or the dispatch stops asking for a real publication.
"""

import functools
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
SHA = "a" * 40
MASTER = "b" * 40


@functools.cache
def handoff_script():
    result = subprocess.run(
        ["yq", "-o=json", ".jobs.crate", str(ROOT / ".github/workflows/proxy-publish.yml")],
        check=True, capture_output=True, text=True,
    )
    runs = [step["run"] for step in json.loads(result.stdout)["steps"] if "run" in step]
    assert len(runs) == 1
    return runs[0]


class ProxyCrateHandoffTests(unittest.TestCase):
    def run_handoff(self, registry="404", diff=0, version="0.5.1", release=SHA, curl_exit=0, gh_exit=0):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        root = Path(temporary.name)
        tools = root / "bin"
        tools.mkdir()
        # Linux Nix sandboxes do not provide /usr/bin/env.
        stand_in = (
            f"#!{sys.executable}\n"
            "import json, os, pathlib, sys\n"
            "name = pathlib.Path(sys.argv[0]).name\n"
            "with open(os.environ['CALLS'], 'a') as calls:\n"
            "    calls.write(json.dumps([name] + sys.argv[1:]) + '\\n')\n"
            "if name == 'curl':\n"
            "    sys.stdout.write(os.environ['REGISTRY_STATUS'])\n"
            "    sys.exit(int(os.environ['CURL_EXIT']))\n"
            "if name == 'git' and sys.argv[1] == 'rev-parse':\n"
            "    print(os.environ['MASTER_SHA'])\n"
            "    sys.exit(0)\n"
            "if name == 'git' and sys.argv[1] == 'diff':\n"
            "    sys.exit(int(os.environ['DIFF_EXIT']))\n"
            "if name == 'gh':\n"
            "    sys.exit(int(os.environ['GH_EXIT']))\n"
            "sys.exit(97)\n"
        )
        for name in ("curl", "git", "gh"):
            (tools / name).write_text(stand_in)
            (tools / name).chmod(0o755)
        calls, summary = root / "calls", root / "summary"
        calls.write_text("")
        result = subprocess.run(
            ["bash", "-c", handoff_script()], cwd=root, text=True, capture_output=True,
            env={"PATH": f"{tools}:{os.environ['PATH']}", "CALLS": str(calls),
                 "GITHUB_STEP_SUMMARY": str(summary), "GITHUB_REPOSITORY": "MaplePrivacyLabs/Maple",
                 "GH_TOKEN": "fixture", "PROXY_VERSION": version, "RELEASE_SHA": release,
                 "RELEASE_TAG": "v3.4.10", "REGISTRY_STATUS": registry, "CURL_EXIT": str(curl_exit),
                 "DIFF_EXIT": str(diff), "GH_EXIT": str(gh_exit), "MASTER_SHA": MASTER},
        )
        recorded = [json.loads(line) for line in calls.read_text().splitlines()]
        return result, recorded, summary.read_text() if summary.exists() else ""

    def dispatches(self, recorded):
        return [call for call in recorded if call[0] == "gh"]

    def test_a_missing_version_with_unchanged_source_starts_one_real_publication(self):
        result, recorded, summary = self.run_handoff()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.dispatches(recorded), [[
            "gh", "workflow", "run", "sdk-publish-rust.yml", "--repo", "MaplePrivacyLabs/Maple",
            "--ref", "master", "--raw-field", "package=maple-proxy", "--raw-field", "version=0.5.1",
            "--raw-field", "mode=trusted", "--raw-field", "dry_run=false",
        ]])
        self.assertIn("maple-proxy 0.5.1", summary)
        self.assertIn("sdk-crates", summary)

    def test_the_registry_is_asked_about_exactly_this_version(self):
        _, recorded, _ = self.run_handoff()
        curl = next(call for call in recorded if call[0] == "curl")
        self.assertEqual(curl[-1], "https://crates.io/api/v1/crates/maple-proxy/0.5.1")

    def test_the_release_is_compared_with_master_over_the_crate_source(self):
        _, recorded, _ = self.run_handoff()
        diff = next(call for call in recorded if call[:2] == ["git", "diff"])
        self.assertEqual(diff, ["git", "diff", "--quiet", SHA, MASTER, "--", "proxy/Cargo.toml",
                                "proxy/Cargo.lock", "proxy/src", "proxy/examples", "proxy/tests"])

    def test_a_version_already_on_the_registry_starts_nothing(self):
        result, recorded, summary = self.run_handoff(registry="200")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.dispatches(recorded), [])
        self.assertIn("already on crates.io", summary)

    def test_only_an_exact_404_means_the_version_is_absent(self):
        for registry, curl_exit in (("500", 0), ("403", 0), ("429", 0), ("301", 0), ("000", 0), ("", 0),
                                    ("404", 7), ("000", 28)):
            with self.subTest(registry=registry, curl_exit=curl_exit):
                result, recorded, _ = self.run_handoff(registry=registry, curl_exit=curl_exit)
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(self.dispatches(recorded), [])

    def test_changed_or_uncomparable_crate_source_starts_nothing(self):
        for diff in (1, 2, 128, 129):
            with self.subTest(diff=diff):
                result, recorded, _ = self.run_handoff(diff=diff)
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(self.dispatches(recorded), [])
                self.assertIn("v3.4.10", result.stderr)

    def test_malformed_versions_and_release_commits_start_nothing(self):
        for version in ("", "0.5", "v0.5.1", "0.5.1-rc.1", "0.5.1+build", "00.5.1", "0.5.1 --raw-field x=y"):
            with self.subTest(version=version):
                result, recorded, _ = self.run_handoff(version=version)
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(recorded, [])
        for release in ("", "master", "a" * 39, "A" * 40):
            with self.subTest(release=release):
                result, recorded, _ = self.run_handoff(release=release)
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(recorded, [])

    def test_a_refused_dispatch_fails_the_job_without_claiming_a_start(self):
        result, recorded, summary = self.run_handoff(gh_exit=1)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(len(self.dispatches(recorded)), 1)
        self.assertEqual(summary, "")


if __name__ == "__main__":
    unittest.main()
