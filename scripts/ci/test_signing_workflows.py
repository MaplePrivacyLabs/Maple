"""Keep app signing credentials behind GitHub's protected environments.

The server-side branch/tag policies are configured by repository administrators;
these tests prevent a workflow change from accidentally bypassing that boundary.
"""

import functools
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
ENVIRONMENTS = {
    "desktop-signing": {
        "TAURI_SIGNING_PRIVATE_KEY", "TAURI_SIGNING_PRIVATE_KEY_PASSWORD",
        "APPLE_CERTIFICATE", "APPLE_CERTIFICATE_PASSWORD", "APPLE_ID",
        "APPLE_ID_PASSWORD", "APPLE_TEAM_ID", "KEYCHAIN_PASSWORD",
    },
    "apple-signing": {
        "APPLE_API_ISSUER", "APPLE_API_KEY", "APPLE_API_PRIVATE_KEY", "APPLE_TEAM_ID",
    },
    "android-signing": {
        "ANDROID_KEYSTORE_BASE64", "ANDROID_KEY_ALIAS", "ANDROID_KEY_PASSWORD",
    },
    "windows-signing": {
        "TAURI_SIGNING_PRIVATE_KEY", "TAURI_SIGNING_PRIVATE_KEY_PASSWORD",
        "AZURE_ACCOUNT", "AZURE_ARTIFACT_SIGNING_ACCOUNT_NAME",
        "AZURE_ARTIFACT_SIGNING_CERTIFICATE_PROFILE_NAME",
        "AZURE_ARTIFACT_SIGNING_ENDPOINT", "AZURE_ARTIFACT_SIGNING_EXPECTED_SUBJECT",
        "AZURE_CLIENT_ID", "AZURE_ENDPOINT", "AZURE_PROFILE",
        "AZURE_SUBSCRIPTION_ID", "AZURE_TENANT_ID",
    },
    "zapstore-publishing": {"ZAPSTORE_SIGN_WITH"},
}
SIGNING_SECRETS = set.union(*ENVIRONMENTS.values())
CONSUMERS = {
    "desktop-build.yml": {
        "build-macos": "desktop-signing", "build-linux": "desktop-signing",
        "build-windows": "windows-signing",
    },
    "mobile-build.yml": {
        "build-ios": "apple-signing", "submit-ios-testflight": "apple-signing",
    },
    "ios-dev-testflight.yml": {
        "build-ios-dev": "apple-signing", "submit-ios-dev-testflight": "apple-signing",
    },
    "android-build.yml": {"build-android": "android-signing"},
    "release.yml": {
        "build-tauri": "desktop-signing", "build-windows": "windows-signing",
        "build-android": "android-signing", "build-ios": "apple-signing",
    },
    "zapstore-publish.yml": {"publish": "zapstore-publishing"},
    "signing-credentials-check.yml": {name: name for name in ENVIRONMENTS},
}


@functools.cache
def workflows():
    return {
        p.name: json.loads(subprocess.check_output(["yq", "-o=json", ".", str(p)], text=True))
        for p in sorted((ROOT / ".github/workflows").glob("*.yml"))
    }


def signing_references(value):
    return set(re.findall(r"secrets\.([A-Z_0-9]+)", json.dumps(value))) & SIGNING_SECRETS


def check_boundary(job):
    used = signing_references(job)
    if used:
        environment = job.get("environment")
        if not isinstance(environment, str) or environment not in ENVIRONMENTS:
            raise ValueError("signing credentials require a fixed protected environment")
        if not used <= ENVIRONMENTS[environment]:
            raise ValueError("signing credentials are not configured in this environment")


class SigningWorkflowTests(unittest.TestCase):
    def test_every_signing_reference_is_inside_the_correct_environment_job(self):
        for name, config in workflows().items():
            with self.subTest(workflow=name):
                self.assertFalse(signing_references({k: v for k, v in config.items() if k != "jobs"}))
                for job in config["jobs"].values():
                    check_boundary(job)

    def test_existing_builds_keep_their_signing_environments(self):
        for name, jobs in CONSUMERS.items():
            for job, environment in jobs.items():
                with self.subTest(workflow=name, job=job):
                    self.assertEqual(workflows()[name]["jobs"][job]["environment"], environment)

    def test_master_release_and_unsigned_pr_triggers_are_preserved(self):
        for name in ("desktop-build.yml", "android-build.yml"):
            self.assertEqual(workflows()[name]["on"], {"push": {"branches": ["master"]}})
        self.assertEqual(workflows()["release.yml"]["on"], {"release": {"types": ["created"]}})
        for name in ("desktop-pr-build.yml", "mobile-pr-build.yml", "android-pr-build.yml"):
            self.assertFalse(signing_references(workflows()[name]))
            for job in workflows()[name]["jobs"].values():
                self.assertNotIn(job.get("environment", ""), ENVIRONMENTS)

    def test_production_testflight_serializes_export_through_upload_on_trusted_master(self):
        config = workflows()["mobile-build.yml"]
        self.assertEqual(config["on"], {"push": {"branches": ["master"]}, "workflow_dispatch": None})
        self.assertEqual(config["concurrency"], {
            "group": "maple-ios-production-testflight", "cancel-in-progress": False, "queue": "max",
        })
        self.assertNotEqual(config["concurrency"]["group"], workflows()["ios-dev-testflight.yml"]["concurrency"]["group"])
        guard = (
            "github.repository == 'MaplePrivacyLabs/Maple' && "
            "github.ref == 'refs/heads/master' && "
            "(github.event_name == 'push' || github.event_name == 'workflow_dispatch')"
        )
        jobs = config["jobs"]
        for name in ("changes", "build-ios", "submit-ios-testflight", "warm-ios-pr-onnx-cache"):
            self.assertIn(guard, " ".join(jobs[name]["if"].split()))
        self.assertEqual(jobs["build-ios"]["needs"], "changes")
        self.assertEqual(jobs["verify-ios-artifacts"]["needs"], "build-ios")
        self.assertEqual(jobs["submit-ios-testflight"]["needs"], "verify-ios-artifacts")
        for job in jobs.values():
            self.assertNotIn("concurrency", job)
        for name in ("build-ios", "warm-ios-pr-onnx-cache"):
            self.assertIn("always() && !cancelled()", jobs[name]["if"])
            self.assertIn("needs.changes.result != 'success'", jobs[name]["if"])

    def test_manual_testflight_dispatch_forces_a_fresh_build_without_a_path_diff(self):
        steps = workflows()["app-change-detection.yml"]["jobs"]["detect"]["steps"]
        classify = next(step for step in steps if step.get("id") == "classify")
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "outputs"
            subprocess.run(["bash", "-c", classify["run"]], check=True, capture_output=True,
                           cwd=directory, env={**os.environ, "GITHUB_EVENT_NAME": "workflow_dispatch",
                                               "GITHUB_OUTPUT": str(output), "BASE_SHA": "", "HEAD_SHA": ""})
            self.assertEqual(dict(line.split("=", 1) for line in output.read_text().splitlines()), {
                name: "true" for name in ("frontend", "macos", "linux", "windows", "ios", "android", "ios_onnx")
            })

    def test_windows_oidc_keeps_its_federated_environment_identity(self):
        for name in ("desktop-build.yml", "release.yml"):
            job = workflows()[name]["jobs"]["build-windows"]
            self.assertEqual(job["environment"], "windows-signing")
            self.assertEqual(job.get("permissions", workflows()[name]["permissions"])["id-token"], "write")

    def test_dev_testflight_runs_for_every_master_push_and_only_trusted_manual_runs(self):
        config = workflows()["ios-dev-testflight.yml"]
        self.assertEqual(config["on"], {"push": {"branches": ["master"]}, "workflow_dispatch": None})
        self.assertEqual(config["permissions"], {"contents": "read"})
        self.assertEqual(config["concurrency"], {
            "group": "maple-ios-dev-testflight", "cancel-in-progress": False, "queue": "max",
        })
        expected_guard = (
            "github.repository == 'MaplePrivacyLabs/Maple' && "
            "github.ref == 'refs/heads/master' && "
            "(github.event_name == 'push' || github.event_name == 'workflow_dispatch')"
        )
        for job in config["jobs"].values():
            self.assertEqual(" ".join(job["if"].split()), expected_guard)
            self.assertNotIn("strategy", job)
            checkout = job["steps"][0]
            self.assertTrue(checkout["uses"].startswith("actions/checkout@"))
            self.assertEqual(checkout["with"], {
                "ref": "${{ github.sha }}", "persist-credentials": False,
            })

    def test_dev_testflight_build_variant_and_artifacts_cannot_use_production_defaults(self):
        job = workflows()["ios-dev-testflight.yml"]["jobs"]["build-ios-dev"]
        build = next(step for step in job["steps"] if "ios-release.sh" in step.get("run", ""))
        self.assertEqual(build["env"]["MAPLE_IOS_VARIANT"], "dev")
        self.assertEqual(build["env"]["MAPLE_IOS_BUILD_NUMBER"], "${{ steps.identity.outputs.build_number }}")
        self.assertEqual(build["env"]["MAPLE_IOS_DEV_AUTH_ORIGIN"], "${{ vars.MAPLE_IOS_DEV_AUTH_ORIGIN }}")
        self.assertEqual(build["env"]["MAPLE_ENFORCE_IOS_SIGNED_REPRODUCIBILITY"], "1")
        upload = next(step for step in job["steps"] if step.get("uses", "").startswith("actions/upload-artifact@"))
        self.assertEqual(upload["with"]["name"], job["outputs"]["artifact_name"])
        self.assertEqual(upload["with"]["if-no-files-found"], "error")
        paths = upload["with"]["path"].splitlines()
        self.assertIn("apps/maple-research/frontend/src-tauri/target/ios-dev/Maple-Dev.ipa", paths)
        self.assertIn("apps/maple-research/frontend/src-tauri/target/ios-dev/ios-build-profile.json", paths)
        self.assertTrue(all("/ios-dev/" in path for path in paths))
        for step in job["steps"]:
            if "key" in step.get("with", {}):
                self.assertTrue(step["with"]["key"].startswith("maple-dev-"))
                self.assertTrue(all(key.startswith("maple-dev-") for key in step["with"]["restore-keys"].splitlines()))

    def test_dev_testflight_verifies_exact_download_before_exposing_upload_credentials(self):
        job = workflows()["ios-dev-testflight.yml"]["jobs"]["submit-ios-dev-testflight"]
        self.assertEqual(job["needs"], "build-ios-dev")
        self.assertEqual(job["permissions"], {"contents": "read"})
        self.assertEqual(job["env"]["MAPLE_IOS_BUILD_NUMBER"], "${{ needs.build-ios-dev.outputs.build_number }}")
        self.assertEqual(job["env"]["MAPLE_IOS_DEV_AUTH_ORIGIN"], "${{ vars.MAPLE_IOS_DEV_AUTH_ORIGIN }}")
        steps = job["steps"]
        download = next(i for i, step in enumerate(steps) if step.get("uses", "").startswith("actions/download-artifact@"))
        self.assertEqual(steps[download]["with"], {
            "name": "${{ needs.build-ios-dev.outputs.artifact_name }}", "path": "artifacts",
        })
        proof = next(i for i, step in enumerate(steps) if "verify-release-artifacts.sh" in step.get("run", ""))
        profile = next(i for i, step in enumerate(steps) if "ios-build-profile.py" in step.get("run", ""))
        upload = next(i for i, step in enumerate(steps) if "altool --upload-app" in step.get("run", ""))
        self.assertLess(download, proof)
        self.assertLess(proof, profile)
        self.assertLess(profile, upload)
        self.assertEqual(steps[proof]["env"]["MAPLE_ENFORCE_IOS_SIGNED_REPRODUCIBILITY"], "1")
        for index in (proof, profile, upload):
            self.assertNotIn("continue-on-error", steps[index])
            self.assertNotIn("if", steps[index])
        for step in steps[:upload]:
            self.assertFalse(signing_references(step))
        verification = steps[profile]["run"]
        for required in (
            "verify-ipa artifacts/ios-dev/Maple-Dev.ipa --variant dev",
            '--source-sha "${GITHUB_SHA}"', '--build-number "${MAPLE_IOS_BUILD_NUMBER}"',
            '--report "${RUNNER_TEMP}/verified-ios-build-profile.json"',
            'cmp artifacts/ios-dev/ios-build-profile.json "${RUNNER_TEMP}/verified-ios-build-profile.json"',
        ):
            self.assertIn(required, verification)
        self.assertIn("--file artifacts/ios-dev/Maple-Dev.ipa", steps[upload]["run"])
        self.assertIn("trap 'rm -f", steps[upload]["run"])

    def test_dev_testflight_has_no_release_or_external_tester_distribution_action(self):
        config = workflows()["ios-dev-testflight.yml"]
        for job in config["jobs"].values():
            for step in job["steps"]:
                self.assertNotRegex(step.get("run", ""), r"gh release|betaGroups|betaTesters|appStoreVersions")
                if "uses" in step:
                    self.assertRegex(step["uses"], r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+@[0-9a-f]{40}$")

    def test_credential_check_is_manual_and_cannot_sign_or_publish(self):
        config = workflows()["signing-credentials-check.yml"]
        self.assertEqual(set(config["on"]), {"workflow_dispatch"})
        self.assertEqual(config["permissions"], {"contents": "read"})
        for job in config["jobs"].values():
            self.assertNotIn("permissions", job)
            self.assertEqual(len(job["steps"]), 1)
            step = job["steps"][0]
            self.assertEqual(step["shell"], "python")
            self.assertEqual(step["run"].splitlines()[0], "import os")
            self.assertNotRegex(step["run"], r"subprocess|os\.system|open\(|socket|urllib|requests")

    def test_missing_wrong_and_dynamic_environments_fail_closed(self):
        reference = {"steps": [{"env": {"KEY": "${{ secrets.APPLE_API_PRIVATE_KEY }}"}}]}
        for environment in (None, "pages-preview", "${{ inputs.environment }}", "android-signing"):
            with self.subTest(environment=environment), self.assertRaises(ValueError):
                check_boundary({**reference, "environment": environment})
        check_boundary({**reference, "environment": "apple-signing"})


if __name__ == "__main__":
    unittest.main()
