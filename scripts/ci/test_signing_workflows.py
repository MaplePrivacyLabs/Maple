"""Keep app signing credentials behind GitHub's protected environments.

The server-side branch/tag policies are configured by repository administrators;
these tests prevent a workflow change from accidentally bypassing that boundary.
"""

import functools
import json
from pathlib import Path
import re
import subprocess
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
        for name in ("desktop-build.yml", "mobile-build.yml", "android-build.yml"):
            self.assertEqual(workflows()[name]["on"], {"push": {"branches": ["master"]}})
        self.assertEqual(workflows()["release.yml"]["on"], {"release": {"types": ["created"]}})
        for name in ("desktop-pr-build.yml", "mobile-pr-build.yml", "android-pr-build.yml"):
            self.assertFalse(signing_references(workflows()[name]))
            for job in workflows()[name]["jobs"].values():
                self.assertNotIn(job.get("environment", ""), ENVIRONMENTS)

    def test_windows_oidc_keeps_its_federated_environment_identity(self):
        for name in ("desktop-build.yml", "release.yml"):
            job = workflows()[name]["jobs"]["build-windows"]
            self.assertEqual(job["environment"], "windows-signing")
            self.assertEqual(job.get("permissions", workflows()[name]["permissions"])["id-token"], "write")

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
