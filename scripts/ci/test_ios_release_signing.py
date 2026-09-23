#!/usr/bin/env python3
"""Execute the release script with fake build tools and canary signing inputs.

This checks process environments, key lifetime, and cleanup, not real signing.
The existing profile tests separately exercise actual snapshot/restore logic.
"""

import base64
import json
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import textwrap
import time
import unittest


SCRIPT_DIR = Path(__file__).resolve().parent
KEY_BYTES = b"fixture-private-key-canary\n"
SIGNING_ENV = {
    "APPLE_API_ISSUER": "fixture-issuer-canary",
    "APPLE_API_KEY": "fixture-id-canary",
    "APPLE_DEVELOPMENT_TEAM": "fixture-team-canary",
    "APPLE_TEAM_ID": "fixture-team-canary",
    "APPLE_API_PRIVATE_KEY": base64.b64encode(KEY_BYTES).decode(),
}

# Run the entire checked-in release script; only its tool/build boundaries are
# replaced. Each shim validates its actual inherited process environment.
COMMON = r'''
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
FRONTEND_DIR="${REPO_ROOT}/apps/maple-research/frontend"
TAURI_DIR="${FRONTEND_DIR}/src-tauri"
check_stage() { python3 "${SCRIPT_DIR}/fixture.py" check "$1"; }
check_stage source
use_release_environment() { check_stage production; }
use_pr_environment() { check_stage dev; }
print_source_provenance() { check_stage provenance; }
verify_rust_lockfile() { check_stage lockfile; }
host_os() { printf darwin; }
use_xcode_toolchain() { check_stage toolchain; }
install_frontend_deps() { check_stage dependencies; }
configure_reproducible_build_metadata() { check_stage metadata; }
build_frontend_dist() { check_stage frontend; export MAPLE_FRONTEND_DIST_TREE_SHA256=fixture; }
verify_ios_onnxruntime_manifest() { check_stage onnx; }
print_ios_onnxruntime_hashes() { :; }
write_ios_onnxruntime_reproducibility_manifest() { :; }
remove_build_tree() { rm -rf "$1"; }
repo_relative_path() { printf '%s\n' "${1#"${REPO_ROOT}"/}"; }
print_canonical_ios_app_hash() { check_stage hash-app; printf 'hash fixture\n'; }
print_canonical_ipa_payload_hash() { check_stage hash-ipa; printf 'hash fixture\n'; }
remove_apple_signing_metadata() { check_stage strip; }
write_sha256_manifest() { check_stage artifact-manifest; printf fixture > "$1"; }
print_file_hashes() { :; }
verify_frontend_dist_unchanged() { check_stage done; }
'''

FIXTURE = r'''
import json, os
from pathlib import Path
import signal, stat, sys, time, zipfile

root = Path(__file__).resolve().parents[2]
tauri = root / "apps/maple-research/frontend/src-tauri"
stage, *args = sys.argv[1:]
if stage == "check":
    stage = args[0]
signing = stage == "signed"
env = os.environ
names = ("APPLE_API_ISSUER", "APPLE_API_KEY", "APPLE_API_PRIVATE_KEY",
         "APPLE_API_KEY_PATH", "APPLE_DEVELOPMENT_TEAM", "APPLE_TEAM_ID")
assert not any(name.startswith("ios_signing_") for name in env), "internal variable exported"
assert "APPLE_API_PRIVATE_KEY" not in env, "encoded key inherited"
owned_dirs = list((root / "tmp").glob("maple-ios-signing.*"))
event = {"stage": stage}
if stage == "prepare":
    event["state_dir"] = args[args.index("--state-dir") + 1]
if signing:
    assert env["APPLE_API_ISSUER"] == "fixture-issuer-canary"
    assert env["APPLE_API_KEY"] == "fixture-id-canary"
    assert env["APPLE_DEVELOPMENT_TEAM"] == "fixture-team-canary"
    key = Path(env["APPLE_API_KEY_PATH"])
    assert key.read_bytes() == b"fixture-private-key-canary\n"
    assert stat.S_IMODE(key.stat().st_mode) == 0o600
    if key != root / "borrowed.p8":
        assert len(owned_dirs) == 1 and key.parent == owned_dirs[0]
        assert stat.S_IMODE(key.parent.stat().st_mode) == 0o700
    else:
        assert not owned_dirs
    event["key_path"] = str(key)
else:
    assert not any(name in env for name in names), "signing input inherited"
    assert not any("fixture-private-key-canary" in value or
                   "fixture-issuer-canary" in value or
                   "fixture-team-canary" in value or
                   "fixture-id-canary" in value for value in env.values()), "canary inherited"
    assert not owned_dirs, "decoded key outside signing phase"

with (root / "events.jsonl").open("a") as log:
    log.write(json.dumps(event) + "\n")
if env.get("FIXTURE_FAIL") == stage:
    sys.exit(37)
if stage == "signed" and env.get("FIXTURE_WAIT"):
    (root / "signer-ready").touch()
    time.sleep(30)

if stage in ("unsigned", "signed"):
    build = tauri / "gen/apple/build"
    app = build / "maple_iOS.xcarchive/Products/Applications/Maple.app"
    app.mkdir(parents=True)
    (app / "payload").write_text("fixture")
    if signing:
        with zipfile.ZipFile(build / "Maple.ipa", "w") as archive:
            archive.writestr("Payload/Maple.app/payload", "fixture")
elif stage == "prepare":
    state = Path(args[args.index("--state-dir") + 1])
    (state / "original").write_bytes((root / "project-state").read_bytes())
    (root / "project-state").write_text("prepared")
    (state / "tauri-build-config.json").write_text("{}")
elif stage == "restore":
    state = Path(args[args.index("--state-dir") + 1])
    (root / "project-state").write_bytes((state / "original").read_bytes())
elif stage == "validate-auth-origin":
    print(args[0])
elif stage == "verify-ipa":
    Path(args[args.index("--report") + 1]).write_text("{}")
elif stage == "canonical":
    print("a" * 64 + "  payload")
'''


class SigningBoundaryTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name).resolve()
        self.scripts = self.root / "scripts/ci"
        self.scripts.mkdir(parents=True)
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.tmp = self.root / "tmp"
        self.tmp.mkdir()
        self.tauri = self.root / "apps/maple-research/frontend/src-tauri"
        (self.tauri / "scripts").mkdir(parents=True)
        (self.root / "project-state").write_text("original")
        (self.tauri / "tauri.conf.json").write_text('{"version":"3.4.2"}')
        for name in ("ios-release.sh", "ios-variant.sh"):
            shutil.copy2(SCRIPT_DIR / name, self.scripts / name)
        decoder = re.search(r"^decode_base64_string_to_file\(\) \{.*?^\}",
                            (SCRIPT_DIR / "_common.sh").read_text(), re.M | re.S)
        self.assertIsNotNone(decoder)
        (self.scripts / "_common.sh").write_text(COMMON + decoder.group(0) + "\n")
        (self.scripts / "fixture.py").write_text(FIXTURE)
        (self.scripts / "ios-build-profile.py").write_text(FIXTURE)
        (self.scripts / "canonical-ios-app-hash.py").write_text(
            "import runpy,sys\nsys.argv[1:] = ['canonical']\n"
            "runpy.run_path(__file__.replace('canonical-ios-app-hash.py', 'fixture.py'))\n")
        self.write_tool(self.bin / "python3", f'exec "{sys.executable}" "$@"')
        self.write_tool(self.bin / "xcodebuild", "exit 0")
        self.write_tool(self.bin / "git", "printf '%040d\\n' 1")
        self.write_tool(self.bin / "bun", '''
            stage=signed
            for arg in "$@"; do
              if [ "$arg" = --no-sign ]; then stage=unsigned; fi
            done
            exec python3 "${FIXTURE_ROOT}/scripts/ci/fixture.py" "$stage"
        ''')
        self.write_tool(self.tauri / "scripts/setup-ios-cargo-config.sh",
                        'exec python3 "${FIXTURE_ROOT}/scripts/ci/fixture.py" check setup-cargo')
        # Use an allowlisted environment; no real developer/CI credentials enter
        # the fixture even when the calling shell has them.
        self.env = {"PATH": str(self.bin) + os.pathsep + os.environ["PATH"],
                    "HOME": str(self.root / "home"), "TMPDIR": str(self.tmp),
                    "FIXTURE_ROOT": str(self.root), "MAPLE_IOS_VARIANT": "production",
                    "MAPLE_IOS_DEV_AUTH_ORIGIN": "https://dev.example.test",
                    "MAPLE_IOS_BUILD_NUMBER": "42.1", **SIGNING_ENV}
        # Prove an inherited export attribute cannot re-export internal names.
        self.env["ios_signing_private_key"] = "inherited-canary"

    def write_tool(self, path, content):
        # Copied Nix-store inputs retain read-only mode; replace only this
        # disposable fixture path instead of mutating the source or its mode.
        path.unlink(missing_ok=True)
        path.write_text("#!/usr/bin/env bash\nset -euo pipefail\n" + textwrap.dedent(content))
        path.chmod(0o755)

    def run_release(self, **changes):
        result = subprocess.run(["bash", str(self.scripts / "ios-release.sh")],
                                env=dict(self.env, **changes), capture_output=True,
                                text=True, timeout=15)
        self.assertNotIn(SIGNING_ENV["APPLE_API_PRIVATE_KEY"], result.stdout + result.stderr)
        self.assertNotIn(KEY_BYTES.decode().strip(), result.stdout + result.stderr)
        return result

    def events(self):
        return [json.loads(line) for line in (self.root / "events.jsonl").read_text().splitlines()]

    def assert_cleanup(self, restored=True):
        self.assertEqual(list(self.tmp.glob("maple-ios-signing.*")), [])
        self.assertEqual(list((self.root / "home").glob(".private_keys/*.p8")), [])
        if restored:
            self.assertEqual((self.root / "project-state").read_text(), "original")

    def test_both_variants_keep_credentials_and_key_outside_unsigned_children(self):
        for variant in ("production", "dev"):
            with self.subTest(variant=variant):
                result = self.run_release(MAPLE_IOS_VARIANT=variant)
                self.assertEqual(result.returncode, 0, result.stderr)
                stages = [event["stage"] for event in self.events()]
                for stage in ("source", "dependencies", "frontend", "unsigned", "signed",
                              "verify-ipa", "done", "restore"):
                    self.assertIn(stage, stages)
                self.assertLess(stages.index("unsigned"), stages.index("signed"))
                self.assert_cleanup()
                (self.root / "events.jsonl").unlink()

    def test_frontend_and_unsigned_failures_never_materialize_key(self):
        for stage in ("dependencies", "frontend", "unsigned"):
            with self.subTest(stage=stage):
                result = self.run_release(FIXTURE_FAIL=stage)
                self.assertEqual(result.returncode, 37, result.stderr)
                self.assertNotIn("signed", [event["stage"] for event in self.events()])
                self.assert_cleanup()
                (self.root / "events.jsonl").unlink()

    def test_signed_and_verification_failures_remove_key_and_restore_project(self):
        for stage in ("signed", "verify-ipa"):
            with self.subTest(stage=stage):
                result = self.run_release(FIXTURE_FAIL=stage)
                self.assertEqual(result.returncode, 37, result.stderr)
                self.assert_cleanup()

    def test_decode_failure_cleans_partial_key(self):
        result = self.run_release(APPLE_API_PRIVATE_KEY="!!!!")
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn("signed", [event["stage"] for event in self.events()])
        self.assert_cleanup()

    def test_restore_failure_still_removes_key_and_preserves_snapshot(self):
        result = self.run_release(FIXTURE_FAIL="restore")
        self.assertNotEqual(result.returncode, 0)
        self.assert_cleanup(restored=False)
        snapshot = next(Path(event["state_dir"]) for event in self.events()
                        if event["stage"] == "prepare")
        self.assertEqual((snapshot / "original").read_text(), "original")
        shutil.rmtree(snapshot)

    def test_borrowed_key_is_unchanged_after_success_and_failure(self):
        key = self.root / "borrowed.p8"
        key.write_bytes(KEY_BYTES)
        key.chmod(0o600)
        for failure in ("", "signed", "restore"):
            with self.subTest(failure=failure):
                # Supplied path takes precedence over encoded input.
                result = self.run_release(APPLE_API_KEY_PATH=str(key),
                                          APPLE_API_PRIVATE_KEY="invalid-base64",
                                          FIXTURE_FAIL=failure)
                self.assertEqual(result.returncode == 0, not failure, result.stderr)
                self.assertEqual(key.read_bytes(), KEY_BYTES)
                self.assertEqual(key.stat().st_mode & 0o777, 0o600)
                self.assert_cleanup(restored=failure != "restore")

    def test_signals_remove_owned_key_and_restore_project(self):
        for signum in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
            with self.subTest(signal=signum):
                ready = self.root / "signer-ready"
                ready.unlink(missing_ok=True)
                process = subprocess.Popen(["bash", str(self.scripts / "ios-release.sh")],
                                           env=dict(self.env, FIXTURE_WAIT="1"),
                                           stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                           text=True, start_new_session=True)
                try:
                    deadline = time.monotonic() + 10
                    while not ready.exists() and process.poll() is None and time.monotonic() < deadline:
                        time.sleep(0.02)
                    self.assertTrue(ready.exists(), "fixture signer did not start")
                    os.killpg(process.pid, signum)
                    stdout, stderr = process.communicate(timeout=10)
                    self.assertEqual(process.returncode, 128 + signum, stderr)
                    self.assertNotIn(KEY_BYTES.decode().strip(), stdout + stderr)
                    self.assert_cleanup()
                finally:
                    if process.poll() is None:
                        os.killpg(process.pid, signal.SIGKILL)
                    process.communicate()

    def test_rehearsal_scopes_ios_credentials_to_ios_release(self):
        shutil.copy2(SCRIPT_DIR / "signed-release-rehearsal.sh", self.scripts)
        (self.scripts / "_common.sh").write_text('host_os() { printf darwin; }\n')
        checker = self.scripts / "rehearsal-check.py"
        checker.write_text('''
import os, sys
from pathlib import Path
stage = sys.argv[1]
names = ("APPLE_API_ISSUER", "APPLE_API_KEY", "APPLE_API_PRIVATE_KEY",
         "APPLE_API_KEY_PATH", "APPLE_DEVELOPMENT_TEAM")
if stage == "ios-release":
    assert os.environ["APPLE_API_PRIVATE_KEY"]
    assert os.environ["APPLE_DEVELOPMENT_TEAM"]
else:
    assert not any(name in os.environ for name in names)
if stage == "desktop-release":
    assert os.environ["APPLE_TEAM_ID"]
with (Path(os.environ["FIXTURE_ROOT"]) / "rehearsal-events").open("a") as log:
    log.write(stage + "\\n")
''')
        for name in ("ios-onnxruntime", "ios-release", "desktop-release", "android-release"):
            self.write_tool(self.scripts / (name + ".sh"),
                            f'exec python3 "{checker}" {name}')
        environment = dict(self.env, MAPLE_RELEASE_FAKE_SIGNING="1",
                           APPLE_CERTIFICATE="fixture", APPLE_CERTIFICATE_PASSWORD="fixture",
                           APPLE_ID="fixture", APPLE_PASSWORD="fixture")
        for target in ("ios", "all", "android"):
            with self.subTest(target=target):
                result = subprocess.run(["bash", str(self.scripts / "signed-release-rehearsal.sh"), target],
                                        env=environment, capture_output=True, text=True, timeout=10)
                self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((self.root / "rehearsal-events").read_text().splitlines(),
                         ["ios-onnxruntime", "ios-release", "desktop-release",
                          "ios-onnxruntime", "ios-release", "android-release"])


if __name__ == "__main__":
    unittest.main()
