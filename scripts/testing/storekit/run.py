#!/usr/bin/env python3
"""Build only the StoreKit UI runner, then test an exact prebuilt Maple.app."""

import argparse
import base64
import datetime
import hashlib
import json
import os
from pathlib import Path
import plistlib
import re
import signal
import shutil
import subprocess
import sys
import time
import urllib.request


ROOT = Path(__file__).resolve().parents[3]
TESTS = ROOT / "apps/maple-research/frontend/src-tauri/tests/storekit"
SUITE_ORDER = (
    "MapleStoreKitUITests/test01CatalogAndStorefront",
    "MapleStoreKitUITests/test02SigningCertificate",
    "MapleStoreKitUITests/test02PurchaseAcknowledgmentThenFinish",
    "MapleStoreKitUITests/test03FailureRelaunchAndRecovery",
    "MapleStoreKitUITests/test04PendingApprovalUsesListener",
    "MapleStoreKitUITests/test05CancelledPurchase",
)
SUITE = set(SUITE_ORDER)
CAPTURE = "MapleStoreKitCaptureUITests/testExportPublicSigningCertificate"
CATALOG = SUITE_ORDER[0]
CERTIFICATE = SUITE_ORDER[1]
ACKNOWLEDGEMENT_CASES = set(SUITE_ORDER[2:5])
FIXTURE_PORT = 38863
MAPLE_BUNDLE = "cloud.opensecret.maple"
RUNNER_BUNDLE = "cloud.opensecret.maple.storekit.uitests.xctrunner"
FIXTURE_HEADERS = {"X-Maple-StoreKit-Fixture": "1",
                   "X-Maple-Fixture-Control": "storekit-harness"}


def write_json(path, value):
    path.write_text(json.dumps(value, indent=2) + "\n")


def selected_cases(only):
    cases = list(only) if only else list(SUITE_ORDER)
    if len(cases) != len(set(cases)):
        raise ValueError("Select each test case only once")
    if any(case not in SUITE | {CAPTURE} for case in cases):
        raise ValueError("--only must name an existing class/test method exactly")
    if CAPTURE in cases and cases != [CAPTURE]:
        raise ValueError("The capture-only case must run alone")
    return cases


def fixture_policy(cases, certificate=None, bootstrap=False, external=False):
    if sum((certificate is not None, bootstrap, external)) > 1:
        raise ValueError("Select only one fixture mode")
    if CAPTURE in cases:
        if certificate is not None or bootstrap or external:
            raise ValueError("Capture-only must not manage or contact a fixture")
        return "none"
    if all(case == CATALOG for case in cases):
        return "none"
    if bootstrap and any(case in ACKNOWLEDGEMENT_CASES for case in cases):
        raise ValueError("Acknowledgment tests require --fixture-certificate or --fixture-external")
    if certificate is not None:
        return "pinned"
    if bootstrap:
        return "bootstrap"
    if external:
        if sum(case != CATALOG for case in cases) > 1:
            raise ValueError("External fixture mode supports one purchase case per invocation; restart it with a fresh journal between cases")
        return "external"
    raise ValueError("Purchase tests require --fixture-certificate, --fixture-bootstrap, or explicit --fixture-external")


def source_hashes():
    paths = [TESTS / "MapleStoreKitUITests.swift", TESTS / "HarnessHost.swift",
             TESTS / "Maple.storekit", TESTS / "StoreKitHarness.xcodeproj/project.pbxproj",
             TESTS / "StoreKitHarness.xcodeproj/xcshareddata/xcschemes/MapleStoreKitHarness.xcscheme"]
    return {str(path.relative_to(ROOT)): hashlib.sha256(path.read_bytes()).hexdigest()
            for path in paths}


def fixture_status():
    request = urllib.request.Request(
        f"http://127.0.0.1:{FIXTURE_PORT}/__test__/status", headers=FIXTURE_HEADERS)
    with urllib.request.urlopen(request, timeout=2) as response:
        return json.load(response)


def validate_fresh_fixture(status, bootstrap):
    if (status.get("fixture_only") is not True
            or status.get("bootstrap_only") is not bootstrap
            or status.get("mode") != "ok"
            or status.get("acknowledged_transactions") != []):
        raise ValueError("Fixture must have the selected mode and an empty fresh journal")


def validate_external_fixture(status, requires_acknowledgment):
    bootstrap = status.get("bootstrap_only")
    if not isinstance(bootstrap, bool) or (requires_acknowledgment and bootstrap):
        raise ValueError("External acknowledgment tests require a certificate-pinned fixture")
    validate_fresh_fixture(status, bootstrap)


def fixture_listener_pids():
    result = subprocess.run(["/usr/sbin/lsof", "-nP", f"-iTCP:{FIXTURE_PORT}",
                             "-sTCP:LISTEN", "-Fp"], capture_output=True, text=True)
    if result.returncode not in (0, 1):
        raise RuntimeError("Unable to verify fixture port ownership")
    return {int(line[1:]) for line in result.stdout.splitlines()
            if line.startswith("p") and line[1:].isdigit()}


def stop_owned_process(process, grace=5):
    """Stop only a process group this runner created with start_new_session."""
    if process.poll() is None:
        try:
            os.killpg(process.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        try:
            process.wait(timeout=grace)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait(timeout=5)
    return process.returncode


class FixtureProcess:
    """One owned server and a new durable journal for exactly one test case."""

    def __init__(self, directory, certificate=None, bootstrap=False):
        self.directory = directory
        self.certificate = certificate
        self.bootstrap = bootstrap
        self.process = None
        self.log = None
        self.metadata = {"mode": "bootstrap" if bootstrap else "pinned"}

    def __enter__(self):
        try:
            if fixture_listener_pids():
                raise RuntimeError("Fixture port 38863 is occupied; preserve that process and stop it through its owner")
            journal = self.directory / "fixture-acknowledgements.jsonl"
            if journal.exists():
                raise RuntimeError("Refusing to reuse a fixture journal")
            node = shutil.which("node")
            if node is None:
                raise RuntimeError("Node is unavailable; use the pinned Apple Nix shell")
            command = [node, str(ROOT / "scripts/testing/storekit/fixture-server.mjs"),
                       "--local-fixture", "--journal", str(journal)]
            if self.bootstrap:
                command.append("--bootstrap-only")
            else:
                command.extend(["--certificate", str(self.certificate)])
                self.metadata["certificate_sha256"] = hashlib.sha256(self.certificate.read_bytes()).hexdigest()
            self.log = (self.directory / "fixture.log").open("w")
            self.process = subprocess.Popen(command, stdout=self.log, stderr=subprocess.STDOUT,
                                            start_new_session=True)
            self.metadata.update(command=command, pid=self.process.pid, journal=str(journal))
            deadline = time.monotonic() + 10
            while time.monotonic() < deadline:
                if self.process.poll() is not None:
                    raise RuntimeError("Owned fixture exited during startup; see fixture.log")
                # Do not accept readiness from an unrelated process that won a bind race.
                if fixture_listener_pids() == {self.process.pid}:
                    status = fixture_status()
                    validate_fresh_fixture(status, self.bootstrap)
                    self.metadata["initial_status"] = status
                    write_json(self.directory / "fixture-identity.json", self.metadata)
                    return self
                time.sleep(0.1)
            raise RuntimeError("Owned fixture did not become ready within 10 seconds")
        except BaseException:
            self.__exit__(*sys.exc_info())
            raise

    def __exit__(self, *_):
        if self.process is not None:
            try:
                if self.process.poll() is None and fixture_listener_pids() == {self.process.pid}:
                    self.metadata["final_status"] = fixture_status()
            except Exception as error:
                self.metadata["final_status_error"] = type(error).__name__
            finally:
                self.metadata["exit_code"] = stop_owned_process(self.process)
                self.metadata["owned_process_exited"] = self.process.poll() is not None
        if self.log is not None:
            self.log.close()
        write_json(self.directory / "fixture-identity.json", self.metadata)


def validate_test_report(report, expected):
    actual = {}
    def visit(nodes):
        for node in nodes:
            if node.get("nodeType") == "Test Case":
                identifier = node["nodeIdentifier"].removesuffix("()")
                outcome = node.get("result")
                if identifier in actual:
                    actual[identifier] = f"Duplicate results: {actual[identifier]} then {outcome}"
                else:
                    actual[identifier] = outcome
            visit(node.get("children", []))
    visit(report["testNodes"])
    return actual, set(actual) == set(expected) and all(value == "Passed" for value in actual.values())


def bundle_digest(bundle):
    """Hash the complete bundle tree; record symlink targets without following them."""
    digest = hashlib.sha256(b"maple-storekit-bundle-path-type-content-v1\0")

    def field(value):
        digest.update(len(value).to_bytes(8, "big"))
        digest.update(value)

    def visit(directory):
        for path in sorted(directory.iterdir(), key=lambda entry: os.fsencode(entry.name)):
            field(os.fsencode(path.relative_to(bundle).as_posix()))
            if path.is_symlink():
                digest.update(b"L")
                field(os.fsencode(os.readlink(path)))
            elif path.is_dir():
                digest.update(b"D")
                visit(path)
            elif path.is_file():
                digest.update(b"F")
                digest.update(path.stat().st_size.to_bytes(8, "big"))
                with path.open("rb") as source:
                    for chunk in iter(lambda: source.read(1024 * 1024), b""):
                        digest.update(chunk)
            else:
                raise ValueError(f"Unsupported special file in app bundle: {path}")

    visit(bundle)
    return digest.hexdigest()


def replace_testroot(value, directory):
    if isinstance(value, str):
        return value.replace("__TESTROOT__", str(directory))
    if isinstance(value, list):
        return [replace_testroot(item, directory) for item in value]
    if isinstance(value, dict):
        return {key: replace_testroot(item, directory) for key, item in value.items()}
    return value


def export_attachments(result, run, expected):
    returncode = 0
    attachments = run / "attachments"
    exported = run_owned(
        ["/usr/bin/xcrun", "xcresulttool", "export", "attachments", "--path", str(result),
         "--output-path", str(attachments)], run / "attachment-export.log", timeout=30)
    write_json(run / "attachment-export-execution.json", exported)
    if exported["exit_code"] == 0:
        manifest = json.loads((attachments / "manifest.json").read_text())
        for test in manifest:
            for item in test["attachments"]:
                if "MapleStoreKitSigningCertificate.cer" in item["suggestedHumanReadableName"]:
                    certificate = run / "MapleStoreKitSigningCertificate.cer"
                    shutil.copyfile(attachments / item["exportedFileName"], certificate)
                    print(f"Public StoreKit test certificate: {certificate}")
                elif "Maple-accessibility-state" in item["suggestedHumanReadableName"]:
                    # A bounded capture may save its AX snapshot before a
                    # later XCTest/WebKit query fails. Retain this partial
                    # evidence without changing the failed test outcome.
                    snapshot = (attachments / item["exportedFileName"]).read_text()
                    match = re.search(r"label: '(\{\n  \"purchaseStatus\".*?\n\})', value:", snapshot, re.S)
                    if match:
                        diagnostic = json.loads(match.group(1))
                        transaction_count = diagnostic.get("transactionCount")
                        verified_recovery = (
                            diagnostic.get("certificateSource") == "verified_recovery"
                            and diagnostic.get("signedTransactionsPresent") is True
                            and type(transaction_count) is int and transaction_count > 0)
                        if (diagnostic.get("environment") == "Xcode"
                                and (diagnostic.get("purchaseStatus") == "success" or verified_recovery)):
                            certificate = base64.b64decode(diagnostic["localSigningCertificate"], validate=True)
                            (run / "MapleStoreKitSigningCertificate.cer").write_bytes(certificate)
                            safe_keys = {"purchaseStatus", "unfinishedCount", "acknowledgedCount", "listenerCount",
                                         "automaticRecovery", "listenersReady", "busy", "error", "storefront",
                                         "productIDs", "environment", "localSigningCertificate", "purchaseTransactionId",
                                         "purchaseJwsPresent", "transactionCount", "unfinishedTransactionIds",
                                         "signedTransactionsPresent", "listenerTransactionId", "listenerJwsPresent",
                                         "acknowledgedTransactionId", "paymentProvider", "certificateSource"}
                            safe_status = {key: value for key, value in diagnostic.items() if key in safe_keys}
                            (run / "Maple-diagnostic-status.json").write_text(json.dumps(safe_status, indent=2) + "\n")
                            print(f"Public certificate recovered from AX snapshot: {run / 'MapleStoreKitSigningCertificate.cer'}")
    else:
        print("Attachment export failed; the original .xcresult is preserved.", file=sys.stderr)
        returncode = exported["exit_code"]
    if any("SigningCertificate" in case for case in expected) and not (run / "MapleStoreKitSigningCertificate.cer").exists():
        print("The selected certificate test produced no certificate attachment.", file=sys.stderr)
        returncode = returncode or 1
    return returncode


def installed_identity(udid, bundle_id, expected_hash):
    installed = Path(subprocess.check_output(
        ["/usr/bin/xcrun", "simctl", "get_app_container", udid, bundle_id, "app"],
        text=True, timeout=10).strip())
    digest = bundle_digest(installed)
    if digest != expected_hash:
        raise ValueError("Installed Maple bundle differs from the frozen --app artifact")
    return {"installed_app": str(installed), "installed_bundle_sha256": digest}


def run_owned(command, log_path, timeout):
    started = time.monotonic()
    with log_path.open("w") as log:
        process = subprocess.Popen(command, stdout=log, stderr=subprocess.STDOUT,
                                   start_new_session=True)
        timed_out = False
        try:
            code = process.wait(timeout=timeout)
        except (subprocess.TimeoutExpired, KeyboardInterrupt):
            timed_out = True
            try:
                os.killpg(process.pid, signal.SIGINT)
            except ProcessLookupError:
                pass
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                stop_owned_process(process)
            code = 124
    return {"command": command, "pid": process.pid, "exit_code": code,
            "timed_out": timed_out, "owned_process_exited": process.poll() is not None,
            "elapsed_seconds": time.monotonic() - started}


def run_case(case, definition, run, metadata, timeout, certificate=None):
    capture_only = case == CAPTURE
    identity = dict(metadata, only=[case], capture_only=capture_only)
    definition_path = run / "Maple.xctestrun"
    definition_path.write_bytes(plistlib.dumps(definition))
    if capture_only:
        identity.update(installed_identity(identity["udid"], identity["bundle_id"],
                                           identity["bundle_sha256"]))
    write_json(run / "identity.json", identity)
    result = run / "result.xcresult"
    command = ["/usr/bin/xcodebuild", "test-without-building", "-xctestrun", str(definition_path),
               "-destination", f"platform=iOS Simulator,id={identity['udid']}",
               "-parallel-testing-enabled", "NO", "-test-timeouts-enabled", "YES",
               "-maximum-test-execution-time-allowance", "120",
               "-resultBundlePath", str(result), f"-only-testing:MapleStoreKitUITests/{case}"]
    print(f"Testing {case}; artifacts: {run}", flush=True)
    execution = {}
    code = 1
    try:
        execution = run_owned(command, run / "test.log", timeout)
        code = execution["exit_code"]
        if result.exists():
            report = subprocess.run(
                ["/usr/bin/xcrun", "xcresulttool", "get", "test-results", "tests", "--path", str(result)],
                capture_output=True, text=True, timeout=30)
            actual, accepted = {}, False
            if report.returncode == 0:
                (run / "tests.json").write_text(report.stdout)
                actual, accepted = validate_test_report(json.loads(report.stdout), {case})
            write_json(run / "validation.json", {"expected": [case], "actual": actual,
                                                "all_selected_tests_passed": accepted})
            if not accepted:
                code = code or 1
            # Export partial failure evidence too; a failed test remains failed.
            artifact_code = export_attachments(result, run, {case})
            code = code or artifact_code
            exported = run / "MapleStoreKitSigningCertificate.cer"
            if certificate is not None and exported.exists() and exported.read_bytes() != certificate.read_bytes():
                raise ValueError("The native-verified Xcode certificate differs from the configured fixture pin")
        else:
            code = code or 1
        identity.update(installed_identity(identity["udid"], identity["bundle_id"],
                                           identity["bundle_sha256"]))
        if bundle_digest(Path(identity["app"])) != identity["bundle_sha256"]:
            raise ValueError("The input Maple artifact changed during the test")
    finally:
        if not capture_only:
            # Only these exact bundle IDs on this explicit device were launched
            # by this invocation. Never terminate Maple during capture-only.
            execution["application_cleanup"] = []
            for bundle in (identity["bundle_id"], RUNNER_BUNDLE):
                cleanup = {"bundle_id": bundle}
                try:
                    stopped = subprocess.run(
                        ["/usr/bin/xcrun", "simctl", "terminate", identity["udid"], bundle],
                        capture_output=True, text=True, timeout=10)
                    cleanup["terminate_exit_code"] = stopped.returncode
                except subprocess.TimeoutExpired:
                    cleanup["error"] = "termination_timed_out"
                    code = code or 1
                execution["application_cleanup"].append(cleanup)
        write_json(run / "execution.json", execution)
        write_json(run / "identity.json", identity)
    return code


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--app", type=Path, required=True)
    parser.add_argument("--udid", required=True)
    parser.add_argument("--output", type=Path, default=ROOT / ".local/storekit-harness")
    parser.add_argument("--only", action="append", default=[], help="Exact class/test identifier; repeat to select cases")
    parser.add_argument("--skip-build", action="store_true", help="Reuse a runner only when its source and toolchain identity match")
    parser.add_argument("--build-only", action="store_true")
    parser.add_argument("--timeout", type=int, default=180, help="Maximum seconds per isolated XCTest invocation")
    parser.add_argument("--build-timeout", type=int, default=180)
    fixture = parser.add_mutually_exclusive_group()
    fixture.add_argument("--fixture-certificate", type=Path, help="Manage a fresh pinned fixture process/journal for each purchase case")
    fixture.add_argument("--fixture-bootstrap", action="store_true", help="Manage fresh bootstrap fixtures; acknowledgment cases are rejected")
    fixture.add_argument("--fixture-external", action="store_true", help="Use an externally owned fresh fixture for one purchase case")
    args = parser.parse_args()
    if min(args.timeout, args.build_timeout) < 1:
        parser.error("Timeouts must be positive")
    try:
        cases = selected_cases(args.only)
        mode = "none" if args.build_only else fixture_policy(
            cases, args.fixture_certificate, args.fixture_bootstrap, args.fixture_external)
    except ValueError as error:
        parser.error(str(error))
    if not os.environ.get("MAPLE_XCODE_VERSION") or not os.environ.get("DEVELOPER_DIR"):
        parser.error("Invoke run.sh through the pinned Apple Nix shell so use_xcode_toolchain prepares the native environment")
    certificate = args.fixture_certificate.resolve(strict=True) if args.fixture_certificate else None
    os.umask(0o077)
    app = args.app.resolve(strict=True)
    if app.suffix != ".app":
        parser.error("--app must be a built iOS simulator .app bundle")
    info = plistlib.loads((app / "Info.plist").read_bytes())
    if info.get("DTPlatformName") != "iphonesimulator" or info.get("CFBundleIdentifier") != MAPLE_BUNDLE:
        parser.error("--app must be the exact cloud.opensecret.maple iOS Simulator bundle")
    toolchain = {
        "developer_dir": str(Path(os.environ["DEVELOPER_DIR"]).resolve()),
        "xcode_version": subprocess.check_output(["/usr/bin/xcodebuild", "-version"], text=True, timeout=10).strip(),
        "simulator_sdk": subprocess.check_output(["/usr/bin/xcrun", "--sdk", "iphonesimulator", "--show-sdk-version"], text=True, timeout=10).strip(),
    }
    listing = json.loads(subprocess.check_output(
        ["/usr/bin/xcrun", "simctl", "list", "devices", "-j"], text=True, timeout=10))
    devices = [(runtime, device) for runtime, group in listing["devices"].items()
               for device in group if device["udid"] == args.udid]
    if len(devices) != 1 or not devices[0][1].get("isAvailable"):
        parser.error("--udid must select one available simulator explicitly")
    runtime, device = devices[0]
    if not args.build_only and device["state"] != "Booted":
        parser.error("Boot the intended simulator before running; this runner does not boot or choose devices")
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    derived = output / "DerivedData"
    stamp = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%S.%fZ")
    run = output / stamp
    run.mkdir()
    inputs = source_hashes()
    build_identity = {"source_sha256": inputs, "toolchain": toolchain}
    build_manifest = derived / "runner-build-identity.json"
    if args.skip_build:
        if not build_manifest.exists() or json.loads(build_manifest.read_text()) != build_identity:
            parser.error("Runner source/toolchain changed or build identity is missing; rebuild without --skip-build")
    else:
        command = ["/usr/bin/xcodebuild", "build-for-testing", "-project", str(TESTS / "StoreKitHarness.xcodeproj"),
                   "-scheme", "MapleStoreKitHarness", "-configuration", "Debug", "-sdk", "iphonesimulator",
                   "-destination", f"platform=iOS Simulator,id={args.udid}", "-derivedDataPath", str(derived),
                   "CODE_SIGN_IDENTITY=-"]
        execution = run_owned(command, run / "build.log", args.build_timeout)
        write_json(run / "build-execution.json", execution)
        if execution["exit_code"]:
            print(f"Runner build failed; evidence: {run}", file=sys.stderr)
            return execution["exit_code"]
        write_json(build_manifest, build_identity)
    if args.build_only:
        print(f"Runner build complete: {derived}")
        return 0
    products = derived / "Build/Products"
    runs = list(products.glob("MapleStoreKitHarness_*.xctestrun"))
    if len(runs) != 1:
        parser.error(f"Expected one runner .xctestrun in {products}; found {len(runs)}")
    definition = replace_testroot(plistlib.loads(runs[0].read_bytes()), products)
    target = definition["MapleStoreKitUITests"]
    old_app = target["UITargetAppPath"]
    capture_only = cases == [CAPTURE]
    if not capture_only:
        target["UITargetAppPath"] = str(app)
        target["DependentProductPaths"] = [str(app) if item == old_app else item for item in target["DependentProductPaths"]]
    target["BundleIdentifiersForCrashReportEmphasis"] = [MAPLE_BUNDLE, RUNNER_BUNDLE]
    target["ParallelizationEnabled"] = False
    target["SystemAttachmentLifetime"] = "keepAlways"
    target["UserAttachmentLifetime"] = "keepAlways"
    target["PreferredScreenCaptureFormat"] = "screenshots"
    metadata = {
        "app": str(app), "bundle_id": MAPLE_BUNDLE, "udid": args.udid,
        "bundle_sha256": bundle_digest(app), "bundle_digest_algorithm": "sha256-path-type-content-v1",
        "executable_sha256": hashlib.sha256((app / info["CFBundleExecutable"]).read_bytes()).hexdigest(),
        "toolchain": toolchain, "simulator_runtime": runtime, "simulator": device,
        "source_head": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip(),
        "source_status": subprocess.check_output(["git", "status", "--porcelain=v1"], cwd=ROOT, text=True).splitlines(),
        "runner_source_sha256": inputs, "fixture_mode": mode, "cases": cases,
        "runner_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        "fixture_source_sha256": hashlib.sha256((ROOT / "scripts/testing/storekit/fixture-server.mjs").read_bytes()).hexdigest(),
    }
    write_json(run / "identity.json", metadata)
    results = []
    for index, case in enumerate(cases, 1):
        case_dir = run / f"{index:02d}-{case.split('/')[-1]}"
        case_dir.mkdir()
        code = 1
        try:
            if case != CATALOG and mode in ("pinned", "bootstrap"):
                with FixtureProcess(case_dir, certificate, bootstrap=mode == "bootstrap"):
                    code = run_case(case, definition, case_dir, metadata, args.timeout, certificate)
            else:
                if case != CATALOG and mode == "external":
                    status = fixture_status()
                    validate_external_fixture(status, case in ACKNOWLEDGEMENT_CASES)
                    write_json(case_dir / "fixture-external-status.json", status)
                code = run_case(case, definition, case_dir, metadata, args.timeout, certificate)
        except Exception as error:
            # Teardown can fail after run_case returned zero inside the with
            # block. Such a case never counts as a successfully completed run.
            code = code or 1
            write_json(case_dir / "runner-error.json", {"type": type(error).__name__, "message": str(error)})
            print(f"{case}: {type(error).__name__}; see {case_dir}", file=sys.stderr)
        results.append({"case": case, "exit_code": code, "artifacts": str(case_dir)})
        exported = case_dir / "MapleStoreKitSigningCertificate.cer"
        if exported.exists():
            shutil.copyfile(exported, run / exported.name)
        write_json(run / "run-results.json", {"planned": cases, "results": results,
                                             "all_selected_tests_passed": len(results) == len(cases) and all(item["exit_code"] == 0 for item in results)})
        if code:
            break
    print(f"StoreKit artifacts: {run}")
    return next((item["exit_code"] for item in results if item["exit_code"]), 0)


if __name__ == "__main__":
    raise SystemExit(main())
