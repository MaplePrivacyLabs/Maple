"""Keep native Windows updater tests unprivileged and separate from releases."""

import functools
import json
from pathlib import Path
import re
import subprocess
import unittest


ROOT = Path(__file__).resolve().parents[2]
WORKFLOW = "agent-windows-updater-tests.yml"


@functools.cache
def workflow(name=WORKFLOW):
    return json.loads(subprocess.check_output(
        ["yq", "-o=json", ".", str(ROOT / ".github/workflows" / name)], text=True,
    ))


class WindowsUpdaterWorkflowTests(unittest.TestCase):
    def test_native_tests_have_no_release_or_signing_authority(self):
        config = workflow()
        self.assertEqual(config["permissions"], {"contents": "read"})
        self.assertEqual(set(config["on"]), {"pull_request", "push", "workflow_dispatch"})
        self.assertEqual(config["on"]["push"]["branches"], ["master"])
        self.assertEqual(set(config["jobs"]), {"native-updater"})
        job = config["jobs"]["native-updater"]
        self.assertEqual(job["runs-on"], "windows-2025")
        self.assertNotIn("environment", job)
        self.assertNotIn("secrets", job)
        self.assertNotIn("permissions", job)
        self.assertNotIn("strategy", job)
        self.assertLessEqual(job["timeout-minutes"], 35)
        self.assertNotIn("secrets.", json.dumps(config))
        self.assertNotIn("id-token", json.dumps(config))
        commands = "\n".join(step.get("run", "") for step in job["steps"])
        self.assertNotRegex(commands, r"(?:gh\s+(?:release|workflow)|wrangler|publisher\.py|windows-sign-file|artifact-sign|cargo\s+publish)")

    def test_native_toolchain_matches_existing_windows_pin_and_avoids_artifact_cache(self):
        steps = workflow()["jobs"]["native-updater"]["steps"]
        actions = [step["uses"] for step in steps if "uses" in step]
        self.assertTrue(all(re.fullmatch(r"[^@]+@[0-9a-f]{40}", action) for action in actions))
        self.assertFalse(any("cache" in action or "download-artifact" in action for action in actions))
        checkout = next(step for step in steps if step.get("uses", "").startswith("actions/checkout@"))
        self.assertIs(checkout["with"]["persist-credentials"], False)
        rust = next(step for step in steps if step.get("uses", "").startswith("dtolnay/rust-toolchain@"))
        existing = next(step for step in workflow("agent-desktop-build.yml")["jobs"]["windows-build"]["steps"]
                        if step.get("uses", "").startswith("dtolnay/rust-toolchain@"))
        self.assertEqual(rust["uses"], existing["uses"])
        self.assertEqual(rust["with"]["toolchain"], existing["with"]["toolchain"])
        linker = next(step for step in steps if "agent_windows.py" in step.get("run", ""))
        self.assertEqual(linker["working-directory"], "apps/maple-agent")
        self.assertEqual(linker["shell"], "pwsh")
        test = next(step for step in steps if "windows-updater-tests.ps1" in step.get("run", ""))
        self.assertEqual(test["shell"], "pwsh")
        self.assertNotIn("continue-on-error", test)

    def test_relevant_contract_changes_select_native_tests_without_research_packaging(self):
        required = {
            ".github/workflows/agent-windows-updater-tests.yml",
            "scripts/ci/windows-updater-tests.ps1",
            "scripts/ci/test_windows_updater_workflow.py",
            "apps/maple-agent/crates/maple-agent-updater/**",
            "apps/maple-agent/Cargo.toml", "apps/maple-agent/Cargo.lock",
            "apps/maple-agent/app/Cargo.toml",
            "apps/maple-agent/release_profile.rs", "apps/maple-agent/release-profiles.json",
            "apps/maple-agent/app/src/main.rs", "apps/maple-agent/app/src/update.rs",
            "apps/maple-agent/app/src/windows_bootstrap.rs", "apps/maple-agent/app/src/desktop.rs",
            "apps/maple-agent/scripts/windows-release.py", "apps/maple-agent/scripts/windows-installer.nsi",
        }
        for event in ("push", "pull_request"):
            paths = set(workflow()["on"][event]["paths"])
            self.assertTrue(required <= paths)
            self.assertFalse(any(path.startswith(("apps/maple-research/", "services/opensecret/")) for path in paths))

    def test_failures_retain_evidence_and_do_not_claim_signed_lifecycle_qualification(self):
        upload = next(step for step in workflow()["jobs"]["native-updater"]["steps"]
                      if step.get("uses", "").startswith("actions/upload-artifact@"))
        self.assertEqual(upload["if"], "always()")
        self.assertEqual(upload["with"]["if-no-files-found"], "error")
        self.assertIn("github.run_attempt", upload["with"]["name"])
        script = (ROOT / "scripts/ci/windows-updater-tests.ps1").read_text()
        self.assertIn("[System.PlatformID]::Win32NT", script)
        self.assertIn("'--locked'", script)
        self.assertIn("'--test', 'windows_bootstrap_native'", script)
        self.assertIn("'--test-threads=1'", script)
        self.assertIn("Get-AuthenticodeSignature -LiteralPath $unrelated", script)
        self.assertIn("A required native Windows test group is missing", script)
        self.assertIn("without skipped tests", script)
        for field in (
            "official_maple_signature_qualified", "signed_installed_a_to_b_qualified",
            "visible_gui_upgrade_qualified", "paired_remote_upgrade_qualified",
        ):
            self.assertRegex(script, rf"{field}\s*=\s*\$false")
            self.assertNotRegex(script, rf"{field}\s*=\s*\$true")


if __name__ == "__main__":
    unittest.main()
