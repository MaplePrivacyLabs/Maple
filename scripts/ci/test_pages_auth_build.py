"""Release pin tests use synthetic local packages and never install dependencies."""

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parent))
from pages_auth_build import check_sdk_pin


class AuthBuildProfileTests(unittest.TestCase):
    def test_fixed_profiles_replace_inherited_vite_values_and_keep_apex_entry(self):
        common = Path(__file__).resolve().parent / "_common.sh"
        shared = {"VITE_CLIENT_ID": "ba5a14b5-d915-47b1-b7b1-afda52bc5fc6",
                  "VITE_AUTH_ORIGIN": "https://trymaple.ai", "VITE_APP_ORIGIN": "https://trymaple.ai",
                  "VITE_MARKETING_ORIGIN": "https://www.trymaple.ai"}
        for profile, expected in (
            ("pr", {"VITE_OPEN_SECRET_API_URL": "https://enclave.secretgpt.ai",
                    "VITE_OPEN_SECRET_PCR_ENVIRONMENT": "development",
                    "VITE_OS_FLAGS_BASE_URL": "https://flags-dev.opensecret.cloud",
                    "VITE_MAPLE_BILLING_API_URL": "https://billing-dev.opensecret.cloud"}),
            ("release", {"VITE_OPEN_SECRET_API_URL": "https://enclave.trymaple.ai",
                         "VITE_OPEN_SECRET_PCR_ENVIRONMENT": "production",
                         "VITE_OS_FLAGS_BASE_URL": "https://flags.opensecret.cloud",
                         "VITE_MAPLE_BILLING_API_URL": "https://billing.opensecret.cloud"}),
        ):
            with self.subTest(profile=profile):
                expected = {**shared, **expected}
                environment = {"PATH": os.environ["PATH"], "VITE_UNEXPECTED": "synthetic",
                               **{key: "https://inherited.invalid" for key in expected}}
                result = subprocess.run(
                    ["bash", "-c", 'source "$1"; "$2"; "$3" -I -c "$4"', "profile-test",
                     str(common), f"use_{profile}_environment", sys.executable,
                     'import json,os; print(json.dumps({k:v for k,v in os.environ.items() if k.startswith("VITE_")}))'],
                    env=environment, text=True, capture_output=True, check=True,
                )
                self.assertEqual(json.loads(result.stdout), expected)


class AuthSDKPinTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.frontend = Path(self.directory.name) / "frontend"
        self.frontend.mkdir()

    def manifest(self, version, **extra):
        (self.frontend / "package.json").write_text(json.dumps({"dependencies": {"@mapleai/sdk": version}, **extra}))

    def test_exact_callback_capable_versions(self):
        for version in ("4.1.0", "4.1.1", "5.0.0"):
            self.manifest(version)
            self.assertEqual(check_sdk_pin(self.frontend), version)

    def test_local_unpublished_range_and_older_versions_rejected(self):
        for version in ("file:../../../sdk", "link:../../../sdk", "workspace:*", "github:owner/sdk",
                        "^4.1.0", "~4.1.0", "latest", "4.1.0-rc.1", "4.0.9", "04.1.0", None):
            with self.subTest(version=version):
                self.manifest(version)
                with self.assertRaises(ValueError):
                    check_sdk_pin(self.frontend)

    def test_manifest_overrides_cannot_replace_the_pin(self):
        for field in ("overrides", "resolutions"):
            self.manifest("4.1.0", **{field: {"@mapleai/sdk": "file:../../../sdk"}})
            with self.assertRaises(ValueError):
                check_sdk_pin(self.frontend)

    def test_installed_name_version_and_link_destination_must_match(self):
        self.manifest("4.1.0")
        sdk = self.frontend / "node_modules/@mapleai/sdk"
        sdk.mkdir(parents=True)
        for name, version in (("@mapleai/sdk", "4.1.0"), ("wrong", "4.1.0"), ("@mapleai/sdk", "4.0.0")):
            (sdk / "package.json").write_text(json.dumps({"name": name, "version": version}))
            if name == "@mapleai/sdk" and version == "4.1.0":
                self.assertEqual(check_sdk_pin(self.frontend, installed=True), "4.1.0")
            else:
                with self.assertRaises(ValueError):
                    check_sdk_pin(self.frontend, installed=True)
        (sdk / "package.json").unlink()
        sdk.rmdir()
        external = Path(self.directory.name) / "sdk"
        external.mkdir()
        (external / "package.json").write_text(json.dumps({"name": "@mapleai/sdk", "version": "4.1.0"}))
        sdk.symlink_to(external, target_is_directory=True)
        with self.assertRaises(ValueError):
            check_sdk_pin(self.frontend, installed=True)


if __name__ == "__main__":
    unittest.main()
