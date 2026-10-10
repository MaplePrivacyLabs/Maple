"""Release-only selection, Nix pin enforcement and hosted runner regressions."""

from contextlib import redirect_stderr
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch


ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("select_xcode", Path(__file__).with_name("select-xcode.py"))
selector = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(selector)


class XcodeSelectionTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.directory = Path(self.temp.name)
        self.pin = {"version": "26.5", "build": "17F42"}

    def install(self, name):
        developer = self.directory / name / "Contents/Developer"
        (developer / "usr/bin").mkdir(parents=True)
        (developer / "usr/bin/xcodebuild").touch()
        return developer.resolve()

    def result(self, version="26.5", build="17F42", code=0):
        return subprocess.CompletedProcess([], code, f"Xcode {version}\nBuild version {build}\n", "")

    def test_release_path_and_exact_build_are_published(self):
        developer = self.install("Xcode_26.5.app")
        with patch.object(selector.subprocess, "run", return_value=self.result()) as run:
            self.assertEqual(selector.select_xcode(self.pin, self.directory), developer)
        self.assertEqual(run.call_args.args[0], ["/usr/bin/xcodebuild", "-version"])
        self.assertEqual(run.call_args.kwargs["env"]["DEVELOPER_DIR"], str(developer))
        environment = self.directory / "env"
        output = self.directory / "output"
        environment.write_text("existing=value\n")
        selector.publish_selection(developer, self.pin, {"GITHUB_ENV": str(environment), "GITHUB_OUTPUT": str(output)})
        self.assertEqual(environment.read_text(), f"existing=value\nDEVELOPER_DIR={developer}\n")
        self.assertEqual(output.read_text(), "version=26.5\nbuild=17F42\n")

    def test_active_runner_xcode_is_aligned_and_verified_without_environment_override(self):
        developer = self.install("Xcode_26.5.app")
        with patch.dict(os.environ, {"DEVELOPER_DIR": "/fixture/rolling-default"}), \
                patch.object(selector.subprocess, "run") as switch, \
                patch.object(selector.subprocess, "check_output", return_value=str(developer) + "\n") as read:
            selector.activate_xcode(developer)
        self.assertEqual(switch.call_args.args[0],
                         ["/usr/bin/sudo", "/usr/bin/xcode-select", "--switch", str(developer)])
        self.assertIs(switch.call_args.kwargs["check"], True)
        self.assertNotIn("DEVELOPER_DIR", switch.call_args.kwargs["env"])
        self.assertEqual(read.call_args.args[0], ["/usr/bin/xcode-select", "--print-path"])
        self.assertNotIn("DEVELOPER_DIR", read.call_args.kwargs["env"])

    def test_failed_switch_or_active_path_mismatch_never_publishes_outputs(self):
        developer = self.install("Xcode_26.5.app")
        for failure in (subprocess.CalledProcessError(1, ["xcode-select"]),
                        selector.ToolchainError("active path mismatch")):
            with self.subTest(failure=failure), \
                    patch.object(selector, "load_pin", return_value=self.pin), \
                    patch.object(selector, "select_xcode", return_value=developer), \
                    patch.object(selector, "activate_xcode", side_effect=failure), \
                    patch.object(selector, "publish_selection") as publish, redirect_stderr(io.StringIO()):
                self.assertEqual(selector.main(), 1)
                publish.assert_not_called()
        with patch.object(selector.subprocess, "run"), \
                patch.object(selector.subprocess, "check_output", return_value="/fixture/wrong\n"):
            with self.assertRaises(selector.ToolchainError):
                selector.activate_xcode(developer)

    def test_patch_zero_alias_normalizes_cache_version(self):
        developer = self.install("Xcode_26.5.0.app")
        with patch.object(selector.subprocess, "run", return_value=self.result(version="26.5.0")):
            self.assertEqual(selector.select_xcode(self.pin, self.directory), developer)

    def test_wrong_build_version_missing_metadata_and_failed_probe_are_rejected(self):
        self.install("Xcode_26.5.app")
        for result in (self.result(build="17F40b"), self.result(version="26.6"),
                       self.result(code=1), subprocess.CompletedProcess([], 0, "Xcode 26.5\n", "")):
            with self.subTest(result=result), patch.object(selector.subprocess, "run", return_value=result):
                with self.assertRaisesRegex(selector.ToolchainError, "26.5 build 17F42"):
                    selector.select_xcode(self.pin, self.directory)

    def test_beta_install_and_disguised_beta_symlink_are_never_probed(self):
        beta = self.install("Xcode_26.5_beta.app")
        (self.directory / "Xcode_26.5.app").symlink_to(beta.parents[1], target_is_directory=True)
        with patch.object(selector.subprocess, "run") as run:
            with self.assertRaises(selector.ToolchainError):
                selector.select_xcode(self.pin, self.directory)
            run.assert_not_called()

    def test_rolling_default_is_skipped_when_pinned_release_is_available(self):
        self.install("Xcode_26.5.app")
        developer = self.install("Xcode_26.5.0.app")
        with patch.object(selector.subprocess, "run", side_effect=[self.result(version="26.6"), self.result()]):
            self.assertEqual(selector.select_xcode(self.pin, self.directory), developer)

    def test_invalid_pin_rejected_before_any_probe(self):
        path = self.directory / "pin.json"
        for pin in ({"version": "26.5"}, {"version": "26.5\nbad", "build": "17F42"},
                    {"version": "26.5", "build": "17F42\nbad"}, []):
            with self.subTest(pin=pin):
                path.write_text(json.dumps(pin))
                with self.assertRaises(selector.ToolchainError):
                    selector.load_pin(path)


class AppleToolchainContractTests(unittest.TestCase):
    def test_local_resolution_skips_same_version_wrong_build_and_fails_if_no_match(self):
        common = ROOT / "scripts/ci/_common.sh"
        script = r'''
source "$1"
is_valid_xcode_developer_dir() { return 0; }
xcode_version_for_developer_dir() { printf '26.5\n'; }
xcode_build_for_developer_dir() {
  if [ "$1" = /fixture/wrong ]; then printf '17F40b\n'; else printf '17F42\n'; fi
}
xcode-select() { printf '/fixture/default\n'; }
resolve_xcode_developer_dir
'''
        environment = {**os.environ, "DEVELOPER_DIR": "/fixture/wrong",
                       "MAPLE_NIX_XCODE_VERSION": "26.5", "MAPLE_NIX_XCODE_BUILD_VERSION": "17F42"}
        result = subprocess.run(["bash", "-c", script, "fixture", str(common)], env=environment,
                                text=True, capture_output=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), "/Applications/Xcode-26.5.app/Contents/Developer")
        environment["MAPLE_NIX_XCODE_BUILD_VERSION"] = "17F113"
        result = subprocess.run(["bash", "-c", script, "fixture", str(common)], env=environment,
                                text=True, capture_output=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("17F113", result.stderr)

    def test_no_workflow_uses_retired_mac_os_and_every_native_selector_has_checkout(self):
        selectors = []
        for path in sorted((ROOT / ".github/workflows").glob("*.yml")):
            workflow = json.loads(subprocess.check_output(["yq", "-o=json", ".", str(path)], text=True))
            self.assertNotRegex(json.dumps(workflow), r"macos-14(?:-large|-xlarge)?")
            for job_id, job in workflow["jobs"].items():
                steps = job.get("steps", [])
                for index, step in enumerate(steps):
                    if "scripts/ci/select-xcode.py" not in step.get("run", ""):
                        self.assertNotIn("Xcode_26.5_beta", step.get("run", ""))
                        continue
                    selectors.append((path.name, job_id))
                    if "if" in step:
                        self.assertEqual(step["if"], "runner.os == 'macOS'", (path.name, job_id))
                    checkout = [s for s in steps[:index] if s.get("uses", "").startswith("actions/checkout@")]
                    self.assertEqual(len(checkout), 1, (path.name, job_id))
                    self.assertIs(checkout[0]["with"]["persist-credentials"], False)
        self.assertCountEqual(selectors, [
            ("agent-desktop-build.yml", "macos-build"),
            ("agent-desktop-build.yml", "macos"),
            ("agent-desktop-build.yml", "macos-unsigned"),
            ("agent-desktop-build.yml", "verify-macos"),
            ("desktop-build.yml", "build-macos"),
            ("desktop-pr-build.yml", "build-macos"),
            ("desktop-pr-build.yml", "build-research-dev-macos"),
            ("ios-dev-testflight.yml", "build-ios-dev"),
            ("ios-dev-testflight.yml", "submit-ios-dev-testflight"),
            ("mobile-build.yml", "build-ios"),
            ("mobile-build.yml", "submit-ios-testflight"),
            ("mobile-build.yml", "warm-ios-pr-onnx-cache"),
            ("mobile-pr-build.yml", "build-ios"),
            ("release.yml", "build-tauri"),
            ("release.yml", "build-ios"),
            ("release.yml", "verify-macos-desktop-release-artifacts"),
            ("release.yml", "verify-release-artifacts"),
        ])


if __name__ == "__main__":
    unittest.main()
