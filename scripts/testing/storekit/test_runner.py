"""Runner policy and lifecycle checks; no subprocesses, sockets, or simulators.

Run alongside test_bundle_identity.py with unittest discovery. Every external
operation fails closed unless a test explicitly supplies its mocked result.
"""

from contextlib import redirect_stderr, redirect_stdout
import base64
import io
import json
from pathlib import Path
import plistlib
import signal
import subprocess
import tempfile
import unittest
from unittest.mock import Mock, call, patch

import run as runner


def fresh_status(bootstrap=False):
    return {
        "fixture_only": True,
        "bootstrap_only": bootstrap,
        "mode": "ok",
        "acknowledged_transactions": [],
    }


def report_for(*cases):
    return {
        "testNodes": [{
            "nodeType": "Test Suite",
            "children": [
                {"nodeType": "Test Case", "nodeIdentifier": name,
                 "result": result}
                for name, result in cases
            ],
        }],
    }


class IsolatedRunnerTest(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.directory = Path(self.temporary.name)
        self.spawn = self.block(runner.subprocess, "Popen")
        self.command = self.block(runner.subprocess, "run")
        self.output = self.block(runner.subprocess, "check_output")
        self.kill = self.block(runner.os, "killpg")
        self.network = self.block(runner.urllib.request, "urlopen")
        socket_guard = patch("socket.socket", side_effect=AssertionError("Unexpected socket"))
        socket_guard.start()
        self.addCleanup(socket_guard.stop)
        self.sleep = self.block(runner.time, "sleep")

    def block(self, owner, name):
        patcher = patch.object(owner, name, side_effect=AssertionError(f"Unexpected {name}"))
        mocked = patcher.start()
        self.addCleanup(patcher.stop)
        return mocked

    def fake_process(self, pid=41001, exited=False):
        process = Mock(pid=pid, returncode=0 if exited else None)
        process.poll.side_effect = lambda: process.returncode

        def wait(timeout):
            process.returncode = -signal.SIGTERM
            return process.returncode

        process.wait.side_effect = wait
        self.spawn.side_effect = None
        self.spawn.return_value = process
        self.kill.side_effect = None
        return process


class SelectionPolicyTests(IsolatedRunnerTest):
    def test_default_suite_has_stable_order_and_excludes_capture(self):
        self.assertEqual(runner.selected_cases([]), list(runner.SUITE_ORDER))
        self.assertNotIn(runner.CAPTURE, runner.selected_cases([]))

    def test_rejects_unknown_duplicate_and_mixed_capture_cases(self):
        selections = [
            ["MapleStoreKitUITests"],
            [runner.CATALOG + "()"],
            [runner.CATALOG, runner.CATALOG],
            [runner.CAPTURE, runner.CATALOG],
            [runner.CATALOG, runner.CAPTURE],
        ]
        for cases in selections:
            with self.subTest(cases=cases), self.assertRaises(ValueError):
                runner.selected_cases(cases)

    def test_capture_requires_no_fixture_mode(self):
        self.assertEqual(runner.selected_cases([runner.CAPTURE]), [runner.CAPTURE])
        self.assertEqual(runner.fixture_policy([runner.CAPTURE]), "none")
        for options in ({"certificate": Path("pin.cer")},
                        {"bootstrap": True}, {"external": True}):
            with self.subTest(options=options), self.assertRaises(ValueError):
                runner.fixture_policy([runner.CAPTURE], **options)

    def test_catalog_needs_no_fixture_but_purchase_cases_do(self):
        self.assertEqual(runner.fixture_policy([runner.CATALOG]), "none")
        for case in runner.SUITE - {runner.CATALOG}:
            with self.subTest(case=case), self.assertRaises(ValueError):
                runner.fixture_policy([case])

    def test_bootstrap_refuses_every_acknowledgment_case(self):
        for case in runner.ACKNOWLEDGEMENT_CASES:
            with self.subTest(case=case), self.assertRaises(ValueError):
                runner.fixture_policy([runner.CATALOG, case], bootstrap=True)
        self.assertEqual(runner.fixture_policy([runner.CERTIFICATE], bootstrap=True), "bootstrap")

    def test_fixture_modes_are_exclusive_and_pinned_allows_full_suite(self):
        pin = Path("pin.cer")
        for options in ({"certificate": pin, "bootstrap": True},
                        {"certificate": pin, "external": True},
                        {"bootstrap": True, "external": True}):
            with self.subTest(options=options), self.assertRaises(ValueError):
                runner.fixture_policy([runner.CERTIFICATE], **options)
        self.assertEqual(runner.fixture_policy(list(runner.SUITE_ORDER), certificate=pin), "pinned")


class FixtureStatusTests(IsolatedRunnerTest):
    def test_fresh_pinned_and_bootstrap_statuses_are_accepted(self):
        for bootstrap in (False, True):
            with self.subTest(bootstrap=bootstrap):
                runner.validate_fresh_fixture(fresh_status(bootstrap), bootstrap)

    def test_stale_journal_missing_fields_wrong_mode_and_nonbool_flags_fail(self):
        invalid_fields = [
            ("acknowledged_transactions", [{"transaction_id": "previous-run"}]),
            ("acknowledged_transactions", None),
            ("mode", "unavailable"),
            ("fixture_only", False),
            ("fixture_only", 1),
            ("bootstrap_only", True),
            ("bootstrap_only", 0),
        ]
        for key, value in invalid_fields:
            status = dict(fresh_status(), **{key: value})
            with self.subTest(key=key, value=value), self.assertRaises(ValueError):
                runner.validate_fresh_fixture(status, False)
        for key in fresh_status():
            status = fresh_status()
            del status[key]
            with self.subTest(missing=key), self.assertRaises(ValueError):
                runner.validate_fresh_fixture(status, False)

    def test_external_acknowledgment_requires_pinned_fresh_fixture(self):
        runner.validate_external_fixture(fresh_status(), True)
        for status in (fresh_status(True),
                       dict(fresh_status(), acknowledged_transactions=["stale"])):
            with self.subTest(status=status), self.assertRaises(ValueError):
                runner.validate_external_fixture(status, True)

    def test_external_certificate_capture_allows_bootstrap_but_still_requires_freshness(self):
        for bootstrap in (False, True):
            runner.validate_external_fixture(fresh_status(bootstrap), False)
            status = dict(fresh_status(bootstrap), acknowledged_transactions=["stale"])
            with self.subTest(bootstrap=bootstrap), self.assertRaises(ValueError):
                runner.validate_external_fixture(status, False)


class TestReportTests(IsolatedRunnerTest):
    def test_accepts_exact_passed_nested_cases_and_normalizes_parentheses(self):
        expected = {runner.CATALOG, runner.CERTIFICATE}
        report = report_for((runner.CATALOG + "()", "Passed"),
                            (runner.CERTIFICATE, "Passed"))
        actual, accepted = runner.validate_test_report(report, expected)
        self.assertTrue(accepted)
        self.assertEqual(actual, dict.fromkeys(expected, "Passed"))

    def test_missing_empty_skipped_failed_and_extra_tests_are_rejected(self):
        reports = [
            {"testNodes": []},
            report_for((runner.CERTIFICATE, "Passed")),
            report_for((runner.CATALOG, "Skipped")),
            report_for((runner.CATALOG, "Failed")),
            report_for((runner.CATALOG, None)),
            report_for((runner.CATALOG, "Passed"), (runner.CERTIFICATE, "Passed")),
        ]
        for report in reports:
            with self.subTest(report=report):
                self.assertFalse(runner.validate_test_report(report, {runner.CATALOG})[1])

    def test_duplicate_results_cannot_hide_an_earlier_failure(self):
        for results in (("Failed", "Passed"), ("Skipped", "Passed"), ("Passed", "Passed")):
            report = report_for((runner.CATALOG, results[0]),
                                (runner.CATALOG + "()", results[1]))
            with self.subTest(results=results):
                self.assertFalse(runner.validate_test_report(report, {runner.CATALOG})[1])


class FixtureProcessTests(IsolatedRunnerTest):
    def test_occupied_port_does_not_spawn_contact_or_kill_existing_owner(self):
        with patch.object(runner, "fixture_listener_pids", return_value={99999}):
            with self.assertRaisesRegex(RuntimeError, "occupied"):
                with runner.FixtureProcess(self.directory, bootstrap=True):
                    self.fail("Occupied fixture must not enter")
        self.spawn.assert_not_called()
        self.network.assert_not_called()
        self.kill.assert_not_called()

    def test_preexisting_journal_is_not_reused_or_modified(self):
        journal = self.directory / "fixture-acknowledgements.jsonl"
        journal.write_text("previous run\n")
        with patch.object(runner, "fixture_listener_pids", return_value=set()):
            with self.assertRaisesRegex(RuntimeError, "journal"):
                runner.FixtureProcess(self.directory, bootstrap=True).__enter__()
        self.assertEqual(journal.read_text(), "previous run\n")
        self.spawn.assert_not_called()
        self.kill.assert_not_called()

    def test_spawn_failure_closes_log_without_killing_an_unowned_process(self):
        self.spawn.side_effect = OSError("mock spawn failure")
        fixture = runner.FixtureProcess(self.directory, bootstrap=True)
        with patch.object(runner, "fixture_listener_pids", return_value=set()), \
                patch.object(runner.shutil, "which", return_value="/mock/node"):
            with self.assertRaisesRegex(OSError, "mock spawn failure"):
                fixture.__enter__()
        self.assertTrue(fixture.log.closed)
        self.kill.assert_not_called()

    def test_early_process_exit_fails_startup_without_signalling_other_processes(self):
        process = self.fake_process(exited=True)
        fixture = runner.FixtureProcess(self.directory, bootstrap=True)
        with patch.object(runner, "fixture_listener_pids", return_value=set()), \
                patch.object(runner.shutil, "which", return_value="/mock/node"):
            with self.assertRaisesRegex(RuntimeError, "exited during startup"):
                fixture.__enter__()
        self.assertTrue(fixture.log.closed)
        self.kill.assert_not_called()
        process.wait.assert_not_called()

    def test_bind_race_timeout_cleans_only_spawned_group_without_contacting_other_listener(self):
        process = self.fake_process()
        fixture = runner.FixtureProcess(self.directory, bootstrap=True)
        self.sleep.side_effect = None
        with patch.object(runner, "fixture_listener_pids", side_effect=[set(), {99999}, {99999}]), \
                patch.object(runner.shutil, "which", return_value="/mock/node"), \
                patch.object(runner.time, "monotonic", side_effect=[0, 0, 11]):
            with self.assertRaisesRegex(RuntimeError, "did not become ready"):
                fixture.__enter__()
        self.kill.assert_called_once_with(process.pid, signal.SIGTERM)
        self.network.assert_not_called()
        self.assertTrue(fixture.log.closed)

    def test_invalid_initial_status_stops_owned_process_and_preserves_failure(self):
        process = self.fake_process()
        fixture = runner.FixtureProcess(self.directory, bootstrap=True)
        stale = dict(fresh_status(True), acknowledged_transactions=["old-run"])
        with patch.object(runner, "fixture_listener_pids", side_effect=[set(), {process.pid}, {process.pid}]), \
                patch.object(runner.shutil, "which", return_value="/mock/node"), \
                patch.object(runner, "fixture_status", return_value=stale):
            with self.assertRaisesRegex(ValueError, "fresh journal"):
                fixture.__enter__()
        self.kill.assert_called_once_with(process.pid, signal.SIGTERM)
        self.assertTrue(fixture.log.closed)

    def test_successful_fixture_has_new_session_and_cleans_up_on_body_failure(self):
        process = self.fake_process()
        fixture = runner.FixtureProcess(self.directory, bootstrap=True)
        with patch.object(runner, "fixture_listener_pids", side_effect=[set(), {process.pid}, {process.pid}]), \
                patch.object(runner.shutil, "which", return_value="/mock/node"), \
                patch.object(runner, "fixture_status", return_value=fresh_status(True)):
            with self.assertRaisesRegex(RuntimeError, "test failed"):
                with fixture:
                    raise RuntimeError("test failed")
        self.assertTrue(self.spawn.call_args.kwargs["start_new_session"])
        self.assertIn("--bootstrap-only", self.spawn.call_args.args[0])
        self.kill.assert_called_once_with(process.pid, signal.SIGTERM)
        self.assertTrue(fixture.log.closed)
        identity = json.loads((self.directory / "fixture-identity.json").read_text())
        self.assertTrue(identity["owned_process_exited"])
        self.assertEqual(identity["pid"], process.pid)


class RunCaseTests(IsolatedRunnerTest):
    def setUp(self):
        super().setUp()
        self.metadata = {
            "udid": "explicit-simulator-id", "bundle_id": runner.MAPLE_BUNDLE,
            "bundle_sha256": "frozen-bundle", "app": "/mock/Maple.app",
        }
        self.definition = {"MapleStoreKitUITests": {"UITargetAppPath": "/mock/Host.app"}}
        self.owned = self.block(runner, "run_owned")
        self.installed = self.block(runner, "installed_identity")
        self.installed.side_effect = None
        self.installed.return_value = {"installed_bundle_sha256": "frozen-bundle"}
        self.digest = self.block(runner, "bundle_digest")
        self.digest.side_effect = None
        self.digest.return_value = "frozen-bundle"
        self.export = self.block(runner, "export_attachments")
        self.fixture_status = self.block(runner, "fixture_status")
        self.fixture_process = self.block(runner, "FixtureProcess")
        self.fixture_owners = self.block(runner, "fixture_listener_pids")
        stdout = redirect_stdout(io.StringIO())
        stdout.__enter__()
        self.addCleanup(stdout.__exit__, None, None, None)

    def invoke(self, case=runner.CAPTURE):
        return runner.run_case(case, self.definition, self.directory, self.metadata, timeout=5)

    def prepare_result(self, case, status="Passed", exit_code=0):
        (self.directory / "result.xcresult").mkdir()
        self.owned.side_effect = None
        self.owned.return_value = {"exit_code": exit_code, "owned_process_exited": True}

        def command(args, **kwargs):
            self.assertEqual(args[:5], ["/usr/bin/xcrun", "xcresulttool", "get", "test-results", "tests"])
            return subprocess.CompletedProcess(args, 0, json.dumps(report_for((case, status))), "")

        self.command.side_effect = command
        self.export.side_effect = None
        self.export.return_value = 0

    def assert_no_fixture_or_termination(self):
        self.fixture_status.assert_not_called()
        self.fixture_process.assert_not_called()
        self.fixture_owners.assert_not_called()
        self.network.assert_not_called()
        self.kill.assert_not_called()
        for command in self.command.call_args_list:
            self.assertNotIn("simctl", command.args[0])

    def test_capture_pass_does_not_terminate_apps_or_contact_fixture(self):
        self.prepare_result(runner.CAPTURE)
        self.assertEqual(self.invoke(), 0)
        self.assertEqual(self.installed.call_args_list, [
            call("explicit-simulator-id", runner.MAPLE_BUNDLE, "frozen-bundle"),
            call("explicit-simulator-id", runner.MAPLE_BUNDLE, "frozen-bundle"),
        ])
        self.assert_no_fixture_or_termination()
        self.assertNotIn("application_cleanup", json.loads((self.directory / "execution.json").read_text()))

    def test_capture_run_error_still_never_terminates_or_contacts_fixture(self):
        self.owned.side_effect = RuntimeError("mock XCTest launch failure")
        with self.assertRaisesRegex(RuntimeError, "mock XCTest launch failure"):
            self.invoke()
        self.assert_no_fixture_or_termination()
        self.assertTrue((self.directory / "execution.json").exists())

    def test_capture_rejects_preexisting_installed_identity_mismatch_before_launch(self):
        self.installed.side_effect = ValueError("wrong installed app")
        with self.assertRaisesRegex(ValueError, "wrong installed app"):
            self.invoke()
        self.owned.assert_not_called()
        self.assert_no_fixture_or_termination()

    def test_capture_zero_exit_without_result_bundle_fails_closed(self):
        self.owned.side_effect = None
        self.owned.return_value = {"exit_code": 0}
        self.assertNotEqual(self.invoke(), 0)
        self.export.assert_not_called()
        self.assert_no_fixture_or_termination()

    def test_capture_skipped_test_cannot_become_success_from_process_exit_zero(self):
        self.prepare_result(runner.CAPTURE, status="Skipped")
        self.assertNotEqual(self.invoke(), 0)
        self.assert_no_fixture_or_termination()

    def test_capture_failed_xcode_exit_cannot_be_overridden_by_passed_report(self):
        self.prepare_result(runner.CAPTURE, exit_code=65)
        self.assertEqual(self.invoke(), 65)
        self.assert_no_fixture_or_termination()

    def test_capture_rejects_input_bundle_change_after_test(self):
        self.prepare_result(runner.CAPTURE)
        self.digest.return_value = "changed-bundle"
        with self.assertRaisesRegex(ValueError, "artifact changed"):
            self.invoke()
        self.assert_no_fixture_or_termination()

    def test_mutating_case_cleanup_targets_only_selected_device_and_two_owned_bundle_ids(self):
        self.owned.side_effect = RuntimeError("mock launch failure")
        self.command.side_effect = None
        self.command.return_value = subprocess.CompletedProcess([], 0, "", "")
        with self.assertRaisesRegex(RuntimeError, "mock launch failure"):
            self.invoke(runner.CATALOG)
        self.assertEqual([entry.args[0] for entry in self.command.call_args_list], [
            ["/usr/bin/xcrun", "simctl", "terminate", "explicit-simulator-id", runner.MAPLE_BUNDLE],
            ["/usr/bin/xcrun", "simctl", "terminate", "explicit-simulator-id", runner.RUNNER_BUNDLE],
        ])
        self.fixture_process.assert_not_called()
        self.fixture_status.assert_not_called()


class AttachmentExportTests(IsolatedRunnerTest):
    def setUp(self):
        super().setUp()
        self.result = self.directory / "result.xcresult"
        self.result.mkdir()
        self.original = self.result / "preserved-evidence"
        self.original.write_bytes(b"original xcresult")

    def export(self, expected=None):
        with redirect_stderr(io.StringIO()), redirect_stdout(io.StringIO()):
            return runner.export_attachments(
                self.result, self.directory, expected or {runner.CATALOG})

    def test_timeout_escalates_only_owned_process_group_and_fails_case(self):
        process = self.fake_process()
        waits = []

        def wait(timeout):
            waits.append(timeout)
            if len(waits) < 4:
                raise subprocess.TimeoutExpired("mock export", timeout)
            process.returncode = -signal.SIGKILL
            return process.returncode

        process.wait.side_effect = wait
        self.assertEqual(self.export(), 124)
        self.assertEqual(waits, [30, 10, 5, 5])
        self.assertEqual(self.kill.call_args_list, [
            call(process.pid, signal.SIGINT),
            call(process.pid, signal.SIGTERM),
            call(process.pid, signal.SIGKILL),
        ])
        self.assertTrue(self.spawn.call_args.kwargs["start_new_session"])
        self.assertTrue(self.spawn.call_args.kwargs["stdout"].closed)
        execution = json.loads((self.directory / "attachment-export-execution.json").read_text())
        self.assertTrue(execution["timed_out"])
        self.assertTrue(execution["owned_process_exited"])
        self.assertEqual(self.original.read_bytes(), b"original xcresult")
        self.command.assert_not_called()

    def test_cancellation_reaps_export_process_and_returns_nonzero(self):
        process = self.fake_process()
        waits = []

        def wait(timeout):
            waits.append(timeout)
            if len(waits) == 1:
                raise KeyboardInterrupt()
            process.returncode = -signal.SIGINT
            return process.returncode

        process.wait.side_effect = wait
        self.assertNotEqual(self.export(), 0)
        self.assertEqual(waits, [30, 10])
        self.kill.assert_called_once_with(process.pid, signal.SIGINT)
        self.assertTrue(self.spawn.call_args.kwargs["stdout"].closed)
        self.assertEqual(self.original.read_bytes(), b"original xcresult")

    def test_successful_bounded_export_retains_certificate_attachment(self):
        process = self.fake_process()

        def wait(timeout):
            process.returncode = 0
            return 0

        process.wait.side_effect = wait
        attachments = self.directory / "attachments"
        attachments.mkdir()
        (attachments / "public.cer").write_bytes(b"mock public certificate")
        (attachments / "manifest.json").write_text(json.dumps([{
            "attachments": [{
                "suggestedHumanReadableName": "MapleStoreKitSigningCertificate.cer",
                "exportedFileName": "public.cer",
            }],
        }]))
        self.assertEqual(self.export({runner.CERTIFICATE}), 0)
        process.wait.assert_called_once_with(timeout=30)
        self.kill.assert_not_called()
        self.assertEqual((self.directory / "MapleStoreKitSigningCertificate.cer").read_bytes(),
                         b"mock public certificate")

    def export_diagnostic(self, run, diagnostic):
        attachments = run / "attachments"
        attachments.mkdir(parents=True)
        (attachments / "snapshot.txt").write_text(
            "label: '" + json.dumps(diagnostic, indent=2) + "', value: diagnostic")
        (attachments / "manifest.json").write_text(json.dumps([{
            "attachments": [{
                "suggestedHumanReadableName": "Maple-accessibility-state",
                "exportedFileName": "snapshot.txt",
            }],
        }]))
        with patch.object(runner, "run_owned", return_value={"exit_code": 0}), \
                redirect_stderr(io.StringIO()), redirect_stdout(io.StringIO()):
            return runner.export_attachments(self.result, run, {runner.CERTIFICATE})

    def recovery_diagnostic(self, status="idle"):
        return {
            "purchaseStatus": status,
            "environment": "Xcode",
            "certificateSource": "verified_recovery",
            "signedTransactionsPresent": True,
            "transactionCount": 1,
            "localSigningCertificate": base64.b64encode(b"mock recovery certificate").decode(),
        }

    def test_verified_recovery_certificate_preserves_idle_or_failed_purchase_status(self):
        for status in ("idle", "failed"):
            with self.subTest(status=status):
                run = self.directory / status
                diagnostic = self.recovery_diagnostic(status)
                diagnostic["notSafeToExport"] = "omitted"
                self.assertEqual(self.export_diagnostic(run, diagnostic), 0)
                self.assertEqual((run / "MapleStoreKitSigningCertificate.cer").read_bytes(),
                                 b"mock recovery certificate")
                saved = json.loads((run / "Maple-diagnostic-status.json").read_text())
                self.assertEqual(saved["purchaseStatus"], status)
                self.assertEqual(saved["certificateSource"], "verified_recovery")
                self.assertTrue(saved["signedTransactionsPresent"])
                self.assertNotIn("notSafeToExport", saved)

    def test_recovery_export_rejects_missing_verification_wrong_source_and_non_xcode(self):
        invalid_fields = [
            ("certificateSource", None),
            ("certificateSource", "unverified_recovery"),
            ("signedTransactionsPresent", None),
            ("signedTransactionsPresent", False),
            ("signedTransactionsPresent", 1),
            ("transactionCount", None),
            ("transactionCount", 0),
            ("transactionCount", -1),
            ("transactionCount", True),
            ("transactionCount", "1"),
            ("environment", "Sandbox"),
            ("environment", "Production"),
        ]
        for index, (key, value) in enumerate(invalid_fields):
            with self.subTest(key=key, value=value):
                run = self.directory / f"invalid-{index}"
                diagnostic = self.recovery_diagnostic()
                if value is None:
                    del diagnostic[key]
                else:
                    diagnostic[key] = value
                self.assertNotEqual(self.export_diagnostic(run, diagnostic), 0)
                self.assertFalse((run / "MapleStoreKitSigningCertificate.cer").exists())
                self.assertFalse((run / "Maple-diagnostic-status.json").exists())

    def test_successful_purchase_fallback_remains_supported_only_for_xcode(self):
        for environment in ("Xcode", "Sandbox"):
            with self.subTest(environment=environment):
                run = self.directory / environment
                diagnostic = {
                    "purchaseStatus": "success", "environment": environment,
                    "localSigningCertificate": base64.b64encode(b"mock purchase certificate").decode(),
                }
                code = self.export_diagnostic(run, diagnostic)
                self.assertEqual(code == 0, environment == "Xcode")
                self.assertEqual((run / "MapleStoreKitSigningCertificate.cer").exists(),
                                 environment == "Xcode")


class MainOrchestrationTests(IsolatedRunnerTest):
    def test_fixture_teardown_failure_after_passed_case_fails_run_and_stops_remaining_cases(self):
        app = self.directory / "Maple.app"
        app.mkdir()
        (app / "Info.plist").write_bytes(plistlib.dumps({
            "DTPlatformName": "iphonesimulator", "CFBundleIdentifier": runner.MAPLE_BUNDLE,
            "CFBundleExecutable": "Maple",
        }))
        (app / "Maple").write_bytes(b"mock executable")
        output = self.directory / "runs"
        derived = output / "DerivedData"
        products = derived / "Build/Products"
        products.mkdir(parents=True)
        (products / "MapleStoreKitHarness_mock.xctestrun").write_bytes(plistlib.dumps({
            "MapleStoreKitUITests": {
                "UITargetAppPath": "__TESTROOT__/Host.app",
                "DependentProductPaths": ["__TESTROOT__/Host.app"],
            },
        }))
        developer = self.directory / "MockXcode/Contents/Developer"
        sources = {"mock-source": "mock-sha256"}
        (derived / "runner-build-identity.json").write_text(json.dumps({
            "source_sha256": sources,
            "toolchain": {"developer_dir": str(developer.resolve()),
                          "xcode_version": "Mock Xcode", "simulator_sdk": "Mock SDK"},
        }))
        responses = {
            ("/usr/bin/xcodebuild", "-version"): "Mock Xcode\n",
            ("/usr/bin/xcrun", "--sdk", "iphonesimulator", "--show-sdk-version"): "Mock SDK\n",
            ("/usr/bin/xcrun", "simctl", "list", "devices", "-j"): json.dumps({
                "devices": {"mock-runtime": [{
                    "udid": "mock-device", "isAvailable": True, "state": "Booted",
                }]},
            }),
            ("git", "rev-parse", "HEAD"): "mock-head\n",
            ("git", "status", "--porcelain=v1"): "",
        }
        self.output.side_effect = lambda args, **kwargs: responses[tuple(args)]
        argv = ["run.py", "--app", str(app), "--udid", "mock-device",
                "--output", str(output), "--skip-build", "--fixture-bootstrap",
                "--only", runner.CERTIFICATE, "--only", runner.SUITE_ORDER[-1]]
        with patch.object(runner.sys, "argv", argv), \
                patch.dict(runner.os.environ, {"MAPLE_XCODE_VERSION": "mock", "DEVELOPER_DIR": str(developer)}), \
                patch.object(runner.os, "umask"), \
                patch.object(runner, "source_hashes", return_value=sources), \
                patch.object(runner, "FixtureProcess") as fixture, \
                patch.object(runner, "run_case", return_value=0) as run_case, \
                redirect_stdout(io.StringIO()), redirect_stderr(io.StringIO()):
            fixture.return_value.__exit__.side_effect = RuntimeError("mock fixture teardown failed")
            self.assertNotEqual(runner.main(), 0)
        run_case.assert_called_once()
        reports = list(output.glob("*/run-results.json"))
        self.assertEqual(len(reports), 1)
        report = json.loads(reports[0].read_text())
        self.assertFalse(report["all_selected_tests_passed"])
        self.assertEqual(len(report["results"]), 1)
        self.assertNotEqual(report["results"][0]["exit_code"], 0)
        errors = list(reports[0].parent.glob("*/runner-error.json"))
        self.assertEqual(len(errors), 1)
        self.assertEqual(json.loads(errors[0].read_text())["message"], "mock fixture teardown failed")
        self.spawn.assert_not_called()
        self.command.assert_not_called()
        self.kill.assert_not_called()
        self.network.assert_not_called()


if __name__ == "__main__":
    unittest.main()
