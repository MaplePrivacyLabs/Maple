#!/usr/bin/env python3
"""The iOS ONNX Runtime source fetch falls back to the next remote for the pinned commit."""

import os
from pathlib import Path
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
SCRIPTS = ROOT / "apps/maple-research/frontend/src-tauri/scripts"
PINS = SCRIPTS / "onnxruntime-pins.sh"
BUILD_SCRIPTS = ("build-ios-onnxruntime-all.sh", "build-ios-onnxruntime.sh")
GIT_ENV = {
    "PATH": os.environ["PATH"], "GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_NOSYSTEM": "1",
    "GIT_AUTHOR_NAME": "test", "GIT_AUTHOR_EMAIL": "test@example.invalid",
    "GIT_COMMITTER_NAME": "test", "GIT_COMMITTER_EMAIL": "test@example.invalid",
}


def git(cwd, *args):
    return subprocess.run(["git", *args], cwd=cwd, env=GIT_ENV, check=True, text=True,
                          capture_output=True).stdout.strip()


class SourceFetch(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.source = self.root / "source"
        self.source.mkdir()
        git(self.source, "init", "--quiet")
        (self.source / "VERSION_NUMBER").write_text("1.23.2\n")
        git(self.source, "add", "VERSION_NUMBER")
        git(self.source, "commit", "--quiet", "-m", "pinned")
        self.commit = git(self.source, "rev-parse", "HEAD")
        self.work = self.root / "work"
        self.work.mkdir()
        git(self.work, "init", "--quiet")

    def fetch(self, *remotes):
        # Replace only the remote list; the helper under test is the shipped one.
        listing = " ".join(f'"{remote}"' for remote in remotes)
        script = (f'source "{PINS}"\n'
                  f"onnxruntime_ios_source_remotes() {{ printf '%s\\n' {listing}; }}\n"
                  f'onnxruntime_ios_fetch_source_commit "{self.commit}"\n')
        return subprocess.run(["bash", "-c", script], cwd=self.work, env=GIT_ENV, text=True,
                              capture_output=True)

    def test_later_remote_serves_the_commit_when_the_first_is_gone(self):
        result = self.fetch(str(self.root / "gone"), str(self.source))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(git(self.work, "rev-parse", "FETCH_HEAD"), self.commit)
        self.assertIn("gone did not serve", result.stderr)

    def test_first_remote_wins_when_it_serves_the_commit(self):
        result = self.fetch(str(self.source), str(self.root / "gone"))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("did not serve", result.stderr)

    def test_fails_when_no_remote_serves_the_commit(self):
        result = self.fetch(str(self.root / "gone"), str(self.root / "also-gone"))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("No source remote served", result.stderr)

    def test_remote_without_the_commit_is_skipped(self):
        other = self.root / "other"
        other.mkdir()
        git(other, "init", "--quiet")
        (other / "README").write_text("unrelated\n")
        git(other, "add", "README")
        git(other, "commit", "--quiet", "-m", "unrelated")
        result = self.fetch(str(other), str(self.source))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(git(self.work, "rev-parse", "FETCH_HEAD"), self.commit)


class Contract(unittest.TestCase):
    def test_upstream_is_tried_first_and_has_a_fallback(self):
        remotes = subprocess.run(["bash", "-c", f'source "{PINS}"; onnxruntime_ios_source_remotes'],
                                 env=GIT_ENV, text=True, capture_output=True, check=True).stdout.split()
        self.assertEqual(remotes[0], "https://github.com/microsoft/onnxruntime.git")
        self.assertGreater(len(remotes), 1)
        self.assertTrue(all(remote.startswith("https://github.com/") for remote in remotes))

    def test_build_scripts_fetch_through_the_helper_and_still_check_the_commit(self):
        for name in BUILD_SCRIPTS:
            text = (SCRIPTS / name).read_text()
            self.assertIn('onnxruntime_ios_fetch_source_commit "${ORT_COMMIT}" || return 1', text, name)
            self.assertNotIn("git remote add", text, name)
            self.assertNotIn("github.com", text, name)
            self.assertIn('if [ "${actual_commit}" != "${ORT_COMMIT}" ]; then', text, name)


if __name__ == "__main__":
    unittest.main()
