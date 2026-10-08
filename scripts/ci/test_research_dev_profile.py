import copy
import importlib.util
import json
from pathlib import Path
import plistlib
import subprocess
import tempfile
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location("research_dev_profile", Path(__file__).with_name("research-dev-profile.py"))
profile_module = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(profile_module)


class ResearchDevProfileTests(unittest.TestCase):
    def setUp(self):
        self.profile = json.loads(profile_module.PROFILE.read_text())
        self.overlay = json.loads((profile_module.TAURI / "tauri.desktop-dev.conf.json").read_text())

    def test_no_production_identity_updater_or_storage(self):
        profile_module.validate_config(self.profile, self.overlay)
        mutations = [
            lambda o: o.update(identifier="cloud.opensecret.maple"),
            lambda o: o["plugins"]["deep-link"]["desktop"].update(schemes=["cloud.opensecret.maple"]),
            lambda o: o["plugins"]["updater"].update(endpoints=["https://updates.trymaple.ai/latest.json"]),
            lambda o: o["bundle"].update(createUpdaterArtifacts=True),
            lambda o: o["app"]["security"].update(capabilities=["default"]),
        ]
        for mutate in mutations:
            with self.subTest(mutation=mutate):
                invalid = copy.deepcopy(self.overlay)
                mutate(invalid)
                with self.assertRaises(ValueError):
                    profile_module.validate_config(self.profile, invalid)

    def test_checks_actual_package_identity_and_scheme(self):
        with tempfile.TemporaryDirectory() as directory:
            bundle = Path(directory)
            (bundle / "Contents/MacOS").mkdir(parents=True)
            (bundle / "Contents/MacOS/maple").write_bytes(b"fixture executable")
            info = {"CFBundleIdentifier": self.profile["identifier"], "CFBundleName": self.profile["productName"],
                    "CFBundleExecutable": "maple", "CFBundleURLTypes": [{"CFBundleURLSchemes": [self.profile["scheme"]]}]}
            path = bundle / "Contents/Info.plist"
            path.write_bytes(plistlib.dumps(info))
            self.assertEqual(profile_module.verify_bundle(bundle, self.profile)["scheme"], "cloud.opensecret.maple.dev")
            info["CFBundleURLTypes"][0]["CFBundleURLSchemes"] = ["cloud.opensecret.maple"]
            path.write_bytes(plistlib.dumps(info))
            with self.assertRaises(ValueError):
                profile_module.verify_bundle(bundle, self.profile)

    def test_pr_artifact_job_has_no_publication_or_signing_authority(self):
        workflow = json.loads(subprocess.check_output([
            "yq", "-o=json", ".", str(profile_module.ROOT / ".github/workflows/desktop-pr-build.yml")
        ], text=True))
        self.assertEqual(set(workflow["on"]), {"pull_request"})
        self.assertEqual(workflow["permissions"], {"contents": "read"})
        job = workflow["jobs"]["build-research-dev-macos"]
        self.assertEqual(job["needs"], "changes")
        self.assertIn("needs.changes.outputs.macos", job["if"])
        self.assertNotIn("environment", job)
        self.assertNotIn("secrets.", json.dumps(job))
        self.assertTrue(any("./scripts/ci/research-dev-desktop.sh" in step.get("run", "") for step in job["steps"]))
        artifact = next(step["with"] for step in job["steps"] if step.get("uses", "").startswith("actions/upload-artifact@"))
        self.assertEqual(artifact["name"], "maple-research-dev-macos-pr")
        self.assertEqual(artifact["if-no-files-found"], "error")
        self.assertIn("build-profile.json", artifact["path"])
        self.assertIn("maple-research-dev-macos.tar.gz", artifact["path"])

    def test_runtime_audit_rejects_host_dependencies_and_search_paths(self):
        system_loads = "maple:\n\t/usr/lib/libiconv.2.dylib (compatibility version 7.0.0, current version 7.0.0)\n"
        bundle_rpath = "Load command 0\n cmd LC_RPATH\n path @executable_path/../Frameworks (offset 12)\n"
        with patch.object(profile_module.subprocess, "check_output", side_effect=[system_loads, bundle_rpath]):
            profile_module.verify_macos_runtime_paths(Path("fixture/maple"))
        for host_path in ("/nix/store/fixture/lib/libiconv.2.dylib", "/Volumes/runner/lib/libiconv.2.dylib", "/Users/runner/lib/libiconv.2.dylib"):
            with self.subTest(host_path=host_path):
                loads = system_loads.replace("/usr/lib/libiconv.2.dylib", host_path)
                with patch.object(profile_module.subprocess, "check_output", side_effect=[loads, bundle_rpath]):
                    with self.assertRaisesRegex(ValueError, "build-host runtime path"):
                        profile_module.verify_macos_runtime_paths(Path("fixture/maple"))
                commands = bundle_rpath.replace("@executable_path/../Frameworks", str(Path(host_path).parent))
                with patch.object(profile_module.subprocess, "check_output", side_effect=[system_loads, commands]):
                    with self.assertRaisesRegex(ValueError, "build-host runtime path"):
                        profile_module.verify_macos_runtime_paths(Path("fixture/maple"))


if __name__ == "__main__":
    unittest.main()
