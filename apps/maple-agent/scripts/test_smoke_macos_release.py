#!/usr/bin/env python3
"""Controlled process fixtures for credential-free native GUI smoke probing."""

import importlib.util
import json
import os
from pathlib import Path
import plistlib
import subprocess
import sys
import tempfile
import unittest
from unittest import mock


spec = importlib.util.spec_from_file_location(
    "smoke_macos_release", Path(__file__).with_name("smoke-macos-release.py")
)
smoke = importlib.util.module_from_spec(spec)
spec.loader.exec_module(smoke)


class GuiSmokeTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.record = self.root / "process.json"
        self.bundle = self.root / "Maple Agent Dev.app"
        self.binary = self.bundle / "Contents/MacOS/maple-agent"
        self.binary.parent.mkdir(parents=True)
        self.binary.write_text("fixture executable")
        self.binary.chmod(0o755)
        self.plist = self.bundle / "Contents/Info.plist"
        with self.plist.open("wb") as target:
            plistlib.dump({
                "CFBundleExecutable": "maple-agent",
                "CFBundleIdentifier": "cloud.opensecret.maple.agent.dev",
            }, target)

    def fixture(self, behavior):
        program = '''import json, os, pathlib, signal, sys, time
record = pathlib.Path(sys.argv[1])
record.write_text(json.dumps({"pid": os.getpid(), "environment": dict(os.environ), "cwd": os.getcwd()}))
''' + behavior
        return [sys.executable, "-c", program, str(self.record)]

    def assert_cleaned(self):
        state = json.loads(self.record.read_text())
        with self.assertRaises(ProcessLookupError):
            os.kill(state["pid"], 0)
        self.assertFalse(Path(state["cwd"]).exists())
        return state

    def test_success_observes_both_markers_and_isolates_state_and_credentials(self):
        command = self.fixture('''
print("startup: window open at 20 ms", file=sys.stderr, flush=True)
print("startup: first render at 30 ms", file=sys.stderr, flush=True)
time.sleep(30)
''')
        parent = dict(os.environ, APPLE_ID_PASSWORD="fixture-notary-secret",
                      BWS_ACCESS_TOKEN="fixture-bws-secret", GITHUB_TOKEN="fixture-gh-secret",
                      MAPLE_API_KEY="fixture-api-secret", DYLD_INSERT_LIBRARIES="fixture-injection")
        smoke.observe_gui(command, timeout=2, settle=0.1, parent_environment=parent)
        state = self.assert_cleaned()
        environment = state["environment"]
        for key in ("APPLE_ID_PASSWORD", "BWS_ACCESS_TOKEN", "GITHUB_TOKEN", "MAPLE_API_KEY", "DYLD_INSERT_LIBRARIES"):
            self.assertNotIn(key, environment)
        self.assertEqual(environment["MAPLE_DISABLE_UPDATE_CHECK"], "1")
        for key in ("XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_CACHE_HOME", "TMPDIR"):
            self.assertTrue(Path(environment[key]).is_relative_to(state["cwd"]))

    def test_missing_render_times_out_and_cleans_only_the_launched_process(self):
        unrelated = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(30)"])
        self.addCleanup(smoke.stop_process, unrelated)
        command = self.fixture('''
print("startup: window open at 20 ms", file=sys.stderr, flush=True)
time.sleep(30)
''')
        with self.assertRaisesRegex(smoke.SmokeError, "startup timeout"):
            smoke.observe_gui(command, timeout=0.4, settle=0.1)
        self.assert_cleaned()
        self.assertIsNone(unrelated.poll())

    def test_child_exit_after_markers_fails_instead_of_claiming_live_gui(self):
        command = self.fixture('''
print("startup: window open at 20 ms", file=sys.stderr, flush=True)
print("startup: first render at 30 ms", file=sys.stderr, flush=True)
''')
        with self.assertRaisesRegex(smoke.SmokeError, "exited"):
            smoke.observe_gui(command, timeout=2, settle=0.2)
        self.assert_cleaned()

    def test_duplicate_runtime_is_rejected_without_echoing_child_output(self):
        command = self.fixture('''
print("private-fixture-value Class is implemented in both runtimes", file=sys.stderr, flush=True)
time.sleep(30)
''')
        with self.assertRaisesRegex(smoke.SmokeError, "duplicate") as caught:
            smoke.observe_gui(command, timeout=2, settle=0.1)
        self.assertNotIn("private-fixture-value", str(caught.exception))
        self.assert_cleaned()

    def test_marker_split_across_reads_is_detected(self):
        command = self.fixture('''
sys.stderr.write("startup: window "); sys.stderr.flush(); time.sleep(0.1)
sys.stderr.write("open at 20 ms\\nstartup: first render at 30 ms\\n"); sys.stderr.flush()
time.sleep(30)
''')
        smoke.observe_gui(command, timeout=2, settle=0.1)
        self.assert_cleaned()

    def test_stdout_does_not_deadlock_the_probe(self):
        command = self.fixture('''
sys.stdout.write("x" * 1048576); sys.stdout.flush()
print("startup: window open at 20 ms", file=sys.stderr, flush=True)
print("startup: first render at 30 ms", file=sys.stderr, flush=True)
time.sleep(30)
''')
        smoke.observe_gui(command, timeout=2, settle=0.1)
        self.assert_cleaned()

    def test_excessive_stderr_is_bounded_without_printing_it(self):
        command = self.fixture('''
sys.stderr.write("private-fixture-value" * 150000); sys.stderr.flush()
time.sleep(30)
''')
        with self.assertRaisesRegex(smoke.SmokeError, "output limit") as caught:
            smoke.observe_gui(command, timeout=2, settle=0.1)
        self.assertNotIn("private-fixture-value", str(caught.exception))
        self.assert_cleaned()

    def test_timeout_reaps_a_process_which_ignores_sigterm(self):
        command = self.fixture('''
signal.signal(signal.SIGTERM, signal.SIG_IGN)
time.sleep(30)
''')
        with self.assertRaisesRegex(smoke.SmokeError, "startup timeout"):
            smoke.observe_gui(command, timeout=0.4, settle=0.1)
        self.assert_cleaned()

    def test_regular_agent_bundle_and_non_agent_or_symlink_rejection(self):
        self.assertEqual(smoke.bundle_executable(self.bundle), self.binary.resolve())
        outside = self.root / "outside-executable"
        self.binary.rename(outside)
        self.binary.symlink_to(outside)
        with self.assertRaisesRegex(smoke.SmokeError, "regular executable"):
            smoke.bundle_executable(self.bundle)
        self.binary.unlink()
        outside.rename(self.binary)
        with self.plist.open("wb") as target:
            plistlib.dump({"CFBundleExecutable": "maple-agent", "CFBundleIdentifier": "unrelated.app"}, target)
        with self.assertRaisesRegex(smoke.SmokeError, "identifier"):
            smoke.bundle_executable(self.bundle)

    def test_native_darwin_requirement_has_no_cli_bypass(self):
        with mock.patch.object(smoke.platform, "system", return_value="Linux"):
            with mock.patch.object(smoke, "observe_gui") as observe:
                with mock.patch("sys.stderr"):
                    self.assertEqual(smoke.main([str(self.bundle)]), 1)
                observe.assert_not_called()


if __name__ == "__main__":
    unittest.main()
