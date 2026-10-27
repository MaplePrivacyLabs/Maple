"""Keep the native Windows updater tests unprivileged and separate from releases.

They run as ci.yml's agent-windows-updater lane (scripts/ci/lanes.py): the
Windows app typecheck and the native updater tests, from two scripts.
"""

import contextlib
import functools
import io
import json
import os
from pathlib import Path
import re
import stat
import subprocess
import tempfile
import unittest
from unittest import mock

import lanes
import plan
from lanes import NIGHTLY, PR, PUSH, QUEUE


ROOT = Path(__file__).resolve().parents[2]
LANE = next(lane for lane in lanes.LANES if lane.id == "agent-windows-updater")
SCRIPTS = ("scripts/ci/windows-app-typecheck.ps1", "scripts/ci/windows-updater-tests.ps1")
REPORTS = ("maple-windows-app-typecheck", "maple-windows-updater-tests")
RELEASE_TOOLS = r"(?:gh\s+(?:release|workflow)|wrangler|publisher\.py|windows-sign-file|artifact-sign|cargo\s+publish)"


@functools.lru_cache(maxsize=None)
def load(path):
    return json.loads(subprocess.check_output(["yq", "-o=json", ".", str(ROOT / path)], text=True))


def job():
    return load(".github/workflows/ci.yml")["jobs"][LANE.job]


def script(name):
    return (ROOT / "scripts/ci" / name).read_text()


class WindowsUpdaterLaneTests(unittest.TestCase):
    def test_native_tests_have_no_release_or_signing_authority(self):
        config = load(".github/workflows/ci.yml")
        self.assertEqual(config["permissions"], {"contents": "read"})
        for authority in ("environment", "secrets", "permissions", "strategy"):
            self.assertNotIn(authority, job())
        self.assertEqual(job()["needs"], "plan")
        self.assertNotIn("secrets.", json.dumps(job()))
        self.assertNotIn("id-token", json.dumps(job()))
        commands = "\n".join([LANE.run] + [step.get("run", "") for step in job()["steps"]])
        self.assertNotRegex(commands, RELEASE_TOOLS)
        self.assertIn("windows", LANE.runner.label)
        self.assertLessEqual(LANE.timeout, 60)

    def test_native_toolchain_matches_the_windows_pin_and_avoids_artifact_caches(self):
        steps = job()["steps"]
        self.assertIn("./.github/actions/setup-agent-windows", [step.get("uses") for step in steps])
        setup = load(".github/actions/setup-agent-windows/action.yml")["runs"]["steps"]
        rust = next(step for step in setup if step.get("uses", "").startswith("dtolnay/rust-toolchain@"))
        self.assertEqual(rust["with"]["toolchain"], lanes.AGENT_WINDOWS_RUST)
        # Both scripts refuse any other compiler than the pinned one.
        for name in SCRIPTS:
            self.assertIn("'(?m)^release: {}$'".format(re.escape(lanes.AGENT_WINDOWS_RUST)), script(Path(name).name))
        # Native Perl for the static crypto build and the smoke-tested linker.
        commands = "\n".join(step.get("run", "") for step in setup)
        for needed in ("OPENSSL_SRC_PERL", "MSWin32", "agent_windows.py"):
            self.assertIn(needed, commands)
        # Compiled from source: no build cache or other run's artifact is restored.
        actions = [step["uses"] for step in steps if "uses" in step]
        self.assertFalse([action for action in actions if "cache" in action or "download-artifact" in action])
        self.assertFalse(LANE.cache or LANE.restore)
        self.assertEqual(dict(LANE.env).get("RUSTFLAGS"), "-D warnings")

    def test_relevant_contract_changes_select_native_tests_without_research_packaging(self):
        required = (
            "scripts/ci/windows-updater-tests.ps1", "scripts/ci/windows-app-typecheck.ps1",
            "scripts/ci/test_windows_updater_workflow.py", "scripts/ci/agent_windows.py",
            "apps/maple-agent/crates/maple-agent-updater/src/installer/windows_bundle.rs",
            "apps/maple-agent/crates/maple-agent-updater/tests/windows_bootstrap_native.rs",
            "apps/maple-agent/Cargo.toml", "apps/maple-agent/Cargo.lock", "apps/maple-agent/app/Cargo.toml",
            "apps/maple-agent/release_profile.rs", "apps/maple-agent/release-profiles.json",
            "apps/maple-agent/app/src/main.rs", "apps/maple-agent/app/src/profile.rs",
            "apps/maple-agent/app/src/update.rs", "apps/maple-agent/app/src/windows_bootstrap.rs",
            "apps/maple-agent/app/src/desktop.rs", "apps/maple-agent/app/src/desktop/remote_updates.rs",
            "apps/maple-agent/scripts/windows-release.py", "apps/maple-agent/scripts/windows-installer.nsi",
            "apps/maple-agent/scripts/release-info.py",
        )
        for path in required:
            for event in (PR, QUEUE, PUSH):
                with self.subTest(path=path, event=event):
                    chosen = set(plan.select(event, [path]))
                    self.assertIn(LANE.id, chosen)
                    self.assertFalse([lane for lane in chosen if lane.startswith(("research-", "os-"))])
        for path in ("apps/maple-agent/app/src/ui/x.rs", "apps/maple-research/frontend/src/main.tsx",
                     "services/opensecret/src/main.rs", "docs/x.md"):
            with self.subTest(path=path):
                self.assertNotIn(LANE.id, plan.select(PR, [path]))
                self.assertNotIn(LANE.id, plan.select(PUSH, [path]))
        for label in ("ci:windows", "ci:full"):
            self.assertIn(LANE.id, plan.select(PR, ["docs/x.md"], [label]))
        self.assertIn(LANE.id, {lane.id for lane in plan.active(NIGHTLY)})

    def test_both_scripts_run_and_either_failure_fails_the_lane(self):
        with tempfile.TemporaryDirectory() as temporary:
            stub = Path(temporary) / "pwsh"
            log = Path(temporary) / "calls"
            stub.write_text('#!/bin/sh\nprintf "%s %s %s\\n" "$*" "$CARGO_TERM_COLOR" "$RUSTFLAGS" >> "$PWSH_LOG"\n'
                            'case "$*" in *"$PWSH_FAIL"*) [ -n "$PWSH_FAIL" ] && exit 1 ;; esac\nexit 0\n')
            stub.chmod(stub.stat().st_mode | stat.S_IXUSR)
            for failing, expected in (("", 0), (SCRIPTS[0], 1), (SCRIPTS[1], 1)):
                log.write_text("")
                environment = {"PATH": f"{temporary}{os.pathsep}{os.environ['PATH']}",
                               "PWSH_LOG": str(log), "PWSH_FAIL": failing}
                with self.subTest(failing=failing), mock.patch.dict(os.environ, environment), \
                        mock.patch.object(plan.platform, "system", return_value="Linux"), \
                        contextlib.redirect_stdout(io.StringIO()):
                    self.assertEqual(plan.run(LANE.id), expected)
                    calls = log.read_text().splitlines()
                    self.assertEqual([call.split()[:3] for call in calls],
                                     [["-NoProfile", "-File", name] for name in SCRIPTS])
                    self.assertTrue(all(call.endswith("never -D warnings") for call in calls))

    def test_app_typecheck_covers_windows_test_fixtures_without_release_codegen(self):
        source = script("windows-app-typecheck.ps1")
        self.assertIn("@('check', '-p', 'maple-agent-app', '--all-targets', '--locked')", source)
        self.assertNotRegex(source, r"cargo\s+(?:build|test|run)|--release|--no-default-features")
        self.assertIn("$receipt.cargo_exit_code -ne 0", source)
        self.assertIn("$receipt.app_all_targets_typechecked = $true", source)
        for field in ("runtime_tests_executed", "app_code_generated", "release_package_qualified"):
            self.assertRegex(source, rf"{field}\s*=\s*\$false")
            self.assertNotRegex(source, rf"{field}\s*=\s*\$true")
        for binding in ("GITHUB_SHA", "GITHUB_RUN_ID", "GITHUB_RUN_ATTEMPT"):
            self.assertIn(binding, source)

    def test_failures_retain_evidence_and_do_not_claim_signed_lifecycle_qualification(self):
        upload = next(step for step in job()["steps"] if step.get("uses", "").startswith("actions/upload-artifact@"))
        self.assertEqual(upload["if"], "always()")
        self.assertEqual(upload["with"]["if-no-files-found"], "error")
        self.assertIn("github.run_attempt", upload["with"]["name"])
        for report in REPORTS:
            self.assertIn("${{ runner.temp }}/" + report + "/", upload["with"]["path"])
            self.assertIn("Join-Path $env:RUNNER_TEMP '{}'".format(report), "\n".join(map(script, (
                "windows-app-typecheck.ps1", "windows-updater-tests.ps1"))))
        run = next(step for step in job()["steps"] if "plan.py run" in step.get("run", ""))
        self.assertIn(LANE.id, run["run"])
        self.assertNotIn("continue-on-error", run)
        source = script("windows-updater-tests.ps1")
        self.assertIn("[System.PlatformID]::Win32NT", source)
        self.assertIn("'--locked'", source)
        self.assertIn("'--test', 'windows_bootstrap_native'", source)
        self.assertIn("'--test-threads=1'", source)
        self.assertIn("'--locked', '--no-fail-fast', '--lib'", source)
        self.assertIn("Get-AuthenticodeSignature -LiteralPath $unrelated", source)
        self.assertIn("A required native Windows test group is missing", source)
        self.assertIn("without skipped tests", source)
        for case in (
            "native_shared_read_validation_accepts_mapped_image_and_mutation_guard",
            "native_mapped_private_image_reports_append_and_generic_write_admission",
            "native_nsis_inherited_read_acl_accepts_files_but_rejects_directories",
            "native_foreign_file_mutation_rights_remain_rejected",
            "native_python_owner_rights_acl_accepts_files_and_directories",
            "native_owner_rights_sid_is_not_a_trusted_storage_owner",
            "native_owner_rights_acl_still_rejects_foreign_writers",
        ):
            self.assertIn("installer::windows_bundle::windows_fs::tests::" + case, source)
        self.assertIn(
            "installer::windows_bundle::manual_activation_tests::"
            "native_manual_pointer_republication_with_generation_guards_preserves_payloads",
            source,
        )
        self.assertIn(
            "installer::windows_bundle::manual_activation_tests::"
            "native_same_current_manual_activation_is_read_only_with_a_mapped_image",
            source,
        )
        for field in (
            "official_maple_signature_qualified", "signed_installed_a_to_b_qualified",
            "visible_gui_upgrade_qualified", "paired_remote_upgrade_qualified",
        ):
            self.assertRegex(source, rf"{field}\s*=\s*\$false")
            self.assertNotRegex(source, rf"{field}\s*=\s*\$true")


if __name__ == "__main__":
    unittest.main()
