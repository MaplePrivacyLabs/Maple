#!/usr/bin/env python3
"""Hermetic identity, artifact-integrity and native-signing boundary tests."""

import importlib.util
import io
import json
import os
from pathlib import Path
import plistlib
import shlex
import shutil
import subprocess
import sys
import tempfile
import tarfile
import unittest


SCRIPTS = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("release_info", SCRIPTS / "release-info.py")
release_info = importlib.util.module_from_spec(spec)
spec.loader.exec_module(release_info)


class PrebuiltReleaseTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.component = Path(self.temp.name)
        scripts = self.component / "scripts"
        scripts.mkdir()
        for name in ("verify-prebuilt-release.sh", "release-info.py"):
            shutil.copy2(SCRIPTS / name, scripts / name)
        shutil.copy2(SCRIPTS.parent / "release-profiles.json", self.component)
        self.env = dict(os.environ, GIT_CONFIG_GLOBAL=os.devnull, GIT_CONFIG_NOSYSTEM="1")
        self.git("init", "--quiet")
        self.git("add", ".")
        self.git("-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
                 "-c", "commit.gpgsign=false", "commit", "--quiet", "-m", "fixture source")
        self.source = self.git("rev-parse", "HEAD").stdout.strip()
        profile = json.loads((self.component / "release-profiles.json").read_text())["dev"]
        self.info = {**profile, "profile": "dev", "version": "0.1.0",
                     "source_sha": self.source, "git_revision": self.source[:8]}
        self.metadata = self.component / "fixture-build-info.json"
        self.binary = self.component / "target/release/maple-agent"
        self.binary.parent.mkdir(parents=True)
        self.binary.write_text(f"""#!{sys.executable}
import os
from pathlib import Path
import sys
assert sys.argv[1:] == ["--build-info"]
for key in ("APPLE_CERTIFICATE", "APPLE_CERTIFICATE_PASSWORD", "APPLE_ID", "APPLE_ID_PASSWORD",
            "APPLE_PASSWORD", "APPLE_TEAM_ID", "TAURI_SIGNING_PRIVATE_KEY", "TAURI_SIGNING_PRIVATE_KEY_PASSWORD"):
    assert key not in os.environ, "prebuilt probe received a signing credential"
print(Path(os.environ["FIXTURE_BUILD_INFO"]).read_text(), end="")
""")
        # Match permissions produced by actions/download-artifact.
        self.binary.chmod(0o644)
        self.scratch = self.component / "temporary-metadata"
        self.scratch.mkdir()
        self.env.update(FIXTURE_BUILD_INFO=str(self.metadata), TMPDIR=str(self.scratch),
                        APPLE_CERTIFICATE="fixture-certificate", APPLE_CERTIFICATE_PASSWORD="fixture-password",
                        APPLE_ID="fixture-id", APPLE_ID_PASSWORD="fixture-notary-password", APPLE_PASSWORD="fixture-password",
                        APPLE_TEAM_ID="fixture-team", TAURI_SIGNING_PRIVATE_KEY="fixture-key",
                        TAURI_SIGNING_PRIVATE_KEY_PASSWORD="fixture-key-password")

    def git(self, *arguments):
        return subprocess.run(["git", "-C", str(self.component), *arguments], env=self.env,
                              check=True, capture_output=True, text=True)

    def verify(self):
        self.metadata.write_text(json.dumps(self.info))
        result = subprocess.run(["bash", str(self.component / "scripts/verify-prebuilt-release.sh"), "dev"],
                                env=self.env, capture_output=True, text=True)
        self.assertFalse(list(self.scratch.iterdir()), "prebuilt metadata must be cleaned on success or failure")
        for value in ("fixture-certificate", "fixture-password", "fixture-notary-password", "fixture-key"):
            self.assertNotIn(value, result.stdout + result.stderr)
        return result

    def test_downloaded_binary_permissions_source_and_secret_boundary(self):
        result = self.verify()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.binary.stat().st_mode & 0o777, 0o755)
        self.assertIn(self.source, result.stdout)

    def test_wrong_profile_or_self_consistent_wrong_source_is_rejected(self):
        original = self.info.copy()
        for change in ({"profile": "prod"}, {"source_sha": "b" * 40, "git_revision": "b" * 8}):
            with self.subTest(change=change):
                self.info = {**original, **change}
                result = self.verify()
                self.assertNotEqual(result.returncode, 0)
                self.assertNotIn("Verified prebuilt", result.stdout)

    def test_downloaded_binary_must_be_a_regular_file(self):
        target = self.binary.with_name("different-binary")
        self.binary.rename(target)
        self.binary.symlink_to(target.name)
        result = self.verify()
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(target.stat().st_mode & 0o777, 0o644)


class LinuxVerifierExtractionTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.component = self.root / "component"
        scripts = self.component / "scripts"
        scripts.mkdir(parents=True)
        for name in ("verify-release.sh", "release-info.py"):
            shutil.copy2(SCRIPTS / name, scripts / name)
        shutil.copy2(SCRIPTS.parent / "release-profiles.json", self.component)
        profiles = json.loads((self.component / "release-profiles.json").read_text())
        self.info = {**profiles["dev"], "profile": "dev", "version": "0.1.0",
                     "source_sha": "a" * 40, "git_revision": "a" * 8}
        self.artifacts = self.root / "artifacts"
        self.artifacts.mkdir()
        (self.artifacts / "build-info.json").write_text(json.dumps(self.info) + "\n")
        launcher = f"""#!{sys.executable}
import sys
if sys.argv[1:] == ["--build-info"]:
    print({json.dumps(self.info)!r})
elif sys.argv[1:] == ["--version"]:
    print("Maple Agent fixture")
else:
    raise SystemExit("unexpected fixture launcher arguments")
"""
        self.image = self.artifacts / "fixture.AppImage"
        self.image.write_text(f"""#!{sys.executable}
import os
from pathlib import Path
import sys
assert sys.argv[1:] == ["--appimage-extract"]
appdir = Path("squashfs-root")
appdir.mkdir(mode=0o755)
for name, content, mode in (("AppRun", {launcher!r}, int(os.environ["FIXTURE_EXEC_MODE"], 8)),
                             ("resource.txt", "fixture data", int(os.environ["FIXTURE_DATA_MODE"], 8))):
    descriptor = os.open(appdir / name, os.O_WRONLY | os.O_CREAT | os.O_EXCL, mode)
    with os.fdopen(descriptor, "w") as output:
        output.write(content)
""")
        release_info.write_manifest(self.info, self.artifacts, "linux-x86_64", True)
        # Substitute only the audit/runtime closure and host identity; the real
        # verifier still checks the manifest, checksums, profile and source.
        (scripts / "linux-release-appimage.py").write_text("""import os
from pathlib import Path
import sys
assert sys.argv[1] == "audit"
appdir = Path(sys.argv[2])
assert appdir.stat().st_mode & 0o777 == 0o755
assert (appdir / "AppRun").stat().st_mode & 0o7777 == 0o755
assert (appdir / "resource.txt").stat().st_mode & 0o7777 == 0o644
private = appdir.parent / "private-after-extraction"
private.write_text("fixture private metadata")
assert private.stat().st_mode & 0o777 == 0o600, "extraction changed the parent's private umask"
assert appdir.parent.stat().st_mode & 0o777 == 0o700
""")
        commands = self.root / "commands"
        commands.mkdir()
        for name, body in {
            "uname": 'import sys; print("Linux" if sys.argv[1] == "-s" else "x86_64")',
            "git": 'print("a" * 40)',
        }.items():
            path = commands / name
            path.write_text(f"#!{sys.executable}\n{body}\n")
            path.chmod(0o755)
        self.env = dict(os.environ, PATH=str(commands) + os.pathsep + os.environ["PATH"],
                        TMPDIR=str(self.root), FIXTURE_EXEC_MODE="755", FIXTURE_DATA_MODE="644")

    def verify(self):
        result = subprocess.run(["bash", str(self.component / "scripts/verify-release.sh"), "dev",
                                 str(self.artifacts), "--unsigned"], env=self.env, capture_output=True, text=True)
        self.assertFalse(list(self.root.glob("maple-agent-verify.*")), "verifier private files must be cleaned")
        return result

    def test_public_archive_modes_and_parent_private_umask_are_preserved(self):
        result = self.verify()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("verified dev linux-x86_64", result.stdout)

    def test_unsafe_archive_modes_are_not_masked_into_passing_modes(self):
        for variable, mode in (("FIXTURE_EXEC_MODE", "777"), ("FIXTURE_DATA_MODE", "666")):
            with self.subTest(variable=variable):
                self.env[variable] = mode
                result = self.verify()
                self.assertNotEqual(result.returncode, 0)
                self.assertNotIn("verified dev linux-x86_64", result.stdout)
                self.env[variable] = "755" if variable == "FIXTURE_EXEC_MODE" else "644"


class ReleaseMetadataTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.directory = Path(self.temp.name)
        profiles = json.loads((SCRIPTS.parent / "release-profiles.json").read_text())
        self.info = {
            **profiles["dev"], "profile": "dev", "version": "0.1.0",
            "source_sha": "a" * 40, "git_revision": "a" * 8,
        }
        self.path = self.directory / "build-info.json"
        self.write_info()

    def write_info(self):
        self.path.write_text(json.dumps(self.info) + "\n")

    def test_profile_and_source_must_match_the_binary(self):
        release_info.read_info("dev", self.path, "a" * 40)
        for profile, source in (("prod", "a" * 40), ("dev", "b" * 40)):
            with self.subTest(profile=profile, source=source), self.assertRaises(ValueError):
                release_info.read_info(profile, self.path, source)

    def test_dirty_or_incomplete_source_revision_is_rejected(self):
        for revision in ("aaaaaaaa-dirty", "unknown", "bbbbbbbb"):
            self.info["git_revision"] = revision
            self.write_info()
            with self.subTest(revision=revision), self.assertRaises(ValueError):
                release_info.read_info("dev", self.path)

    def test_endpoint_or_state_namespace_drift_is_rejected(self):
        for field, value in (("api_url", "https://wrong.invalid"), ("data_namespace", "maple-agent-prod")):
            original = self.info[field]
            self.info[field] = value
            self.write_info()
            with self.subTest(field=field), self.assertRaises(ValueError):
                release_info.read_info("dev", self.path)
            self.info[field] = original

    def test_metadata_cannot_carry_extra_environment_or_secret_fields(self):
        self.info["APPLE_CERTIFICATE"] = "fixture"
        self.write_info()
        with self.assertRaises(ValueError):
            release_info.read_info("dev", self.path)

    def test_release_plist_has_identity_permissions_and_no_shell_environment(self):
        output = self.directory / "Info.plist"
        release_info.write_plist(self.info, output, "42")
        plist = plistlib.loads(output.read_bytes())
        self.assertEqual(plist["CFBundleIdentifier"], self.info["bundle_id"])
        self.assertEqual(plist["CFBundleVersion"], "42")
        self.assertEqual(plist["LSMinimumSystemVersion"], "15.0")
        self.assertTrue(plist["NSMicrophoneUsageDescription"])
        self.assertTrue(plist["NSScreenCaptureUsageDescription"])
        self.assertNotIn("LSEnvironment", plist)
        with self.assertRaises(ValueError):
            release_info.write_plist(self.info, output, "0")

    def package(self, unsigned=True):
        (self.directory / "fixture.dmg").write_bytes(b"package")
        release_info.write_manifest(self.info, self.directory, "macos-aarch64", unsigned)

    def test_unsigned_package_cannot_pass_master_verification(self):
        self.package()
        release_info.verify_artifacts("dev", self.directory, True)
        with self.assertRaises(ValueError):
            release_info.verify_artifacts("dev", self.directory, False)

    def test_final_downloaded_package_tamper_is_rejected(self):
        self.package(False)
        (self.directory / "fixture.dmg").write_bytes(b"altered package")
        with self.assertRaises(ValueError):
            release_info.verify_artifacts("dev", self.directory, False)

    def test_self_consistent_wrong_source_commit_is_rejected(self):
        self.info["source_sha"] = "b" * 40
        self.info["git_revision"] = "b" * 8
        self.write_info()
        self.package(False)
        release_info.verify_artifacts("dev", self.directory, False)
        with self.assertRaises(ValueError):
            release_info.verify_artifacts("dev", self.directory, False, "a" * 40)

    def test_unchecked_files_symlinks_and_checksum_traversal_are_rejected(self):
        self.package()
        extra = self.directory / "extra"
        extra.write_text("unchecked")
        with self.assertRaises(ValueError):
            release_info.verify_artifacts("dev", self.directory, True)
        extra.unlink()
        extra.symlink_to(self.path)
        with self.assertRaises(ValueError):
            release_info.verify_artifacts("dev", self.directory, True)
        extra.unlink()
        (self.directory / "SHA256SUMS").write_text("a" * 64 + "  ../outside\n")
        with self.assertRaises(ValueError):
            release_info.verify_artifacts("dev", self.directory, True)


class AppArchiveTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.directory = Path(self.temp.name)
        self.archive = self.directory / "fixture.app.tar.gz"
        self.app_name = "Maple Agent Dev"
        self.root = f"{self.app_name}.app"

    def write_archive(self, extra=()):
        members = [(self.root, "directory", ""), (self.root + "/Contents", "directory", ""),
                   (self.root + "/Contents/maple-agent", "file", "fixture")]
        with tarfile.open(self.archive, "w:gz") as archive:
            for name, kind, value in members + list(extra):
                member = tarfile.TarInfo(name)
                member.mode = 0o755 if kind == "directory" else 0o644
                if kind == "directory":
                    member.type = tarfile.DIRTYPE
                    archive.addfile(member)
                elif kind in ("symlink", "hardlink"):
                    member.type = tarfile.SYMTYPE if kind == "symlink" else tarfile.LNKTYPE
                    member.linkname = value
                    archive.addfile(member)
                else:
                    data = value.encode()
                    member.size = len(data)
                    archive.addfile(member, io.BytesIO(data))

    def test_extracts_exact_expected_app_and_compares_every_payload_file(self):
        self.write_archive([(self.root + "/Contents/link", "symlink", "maple-agent")])
        first = release_info.extract_app_archive(self.archive, self.directory / "first", self.app_name)
        second = release_info.extract_app_archive(self.archive, self.directory / "second", self.app_name)
        self.assertEqual(first.stat().st_mode & 0o777, 0o755)
        release_info.compare_app_payloads(first, second)
        (second / "Contents/maple-agent").write_text("different archive binary")
        with self.assertRaises(ValueError):
            release_info.compare_app_payloads(first, second)

    def test_rejects_traversal_unexpected_roots_and_link_escape(self):
        cases = [
            ("/outside", "file", "fixture"),
            (self.root + "/../outside", "file", "fixture"),
            ("Unexpected.app/file", "file", "fixture"),
            (self.root + "/Contents/link", "symlink", "../../outside"),
            (self.root + "/Contents/link", "symlink", "/outside"),
            (self.root + "/Contents/link", "hardlink", self.root + "/Contents/maple-agent"),
        ]
        for index, extra in enumerate(cases):
            self.write_archive([extra])
            with self.subTest(extra=extra), self.assertRaises(ValueError):
                release_info.extract_app_archive(self.archive, self.directory / str(index), self.app_name)

    def test_rejects_members_written_through_internal_symlinks(self):
        self.write_archive([
            (self.root + "/alias", "symlink", "Contents"),
            (self.root + "/alias/file", "file", "fixture"),
        ])
        with self.assertRaises(ValueError):
            release_info.extract_app_archive(self.archive, self.directory / "output", self.app_name)


class NativeSigningBoundaryTests(unittest.TestCase):
    setUp = ReleaseMetadataTests.setUp
    write_info = ReleaseMetadataTests.write_info
    def run_packager(self, unsigned=False, failure="", signal=False):
        binary = self.directory / "maple-agent"
        binary.write_text(f"""#!{sys.executable}
import json,os,sys
assert not any(name in os.environ for name in (
    'APPLE_CERTIFICATE','APPLE_CERTIFICATE_PASSWORD','APPLE_ID','APPLE_ID_PASSWORD','APPLE_TEAM_ID'
)), 'signing credentials reached a binary execution'
print(open(os.environ['FIXTURE_BUILD_INFO']).read(), end='')
""")
        binary.chmod(0o755)
        log = self.directory / "native-calls.jsonl"
        log.unlink(missing_ok=True)
        wrapper = self.directory / "mock-packager.sh"
        wrapper.write_text(f"""#!/usr/bin/env bash
set -euo pipefail
source {shlex.quote(str(SCRIPTS / 'macos-release-app.sh'))}
uname() {{ if [[ "$1" == -s ]]; then echo Darwin; else echo arm64; fi; }}
macos_embed_dylibs() {{ :; }}
macos_native() {{
    python3 - "$@" <<'PY'
import json,os,sys
assert not any(key.startswith('APPLE_') for key in os.environ), 'credential exported to a native probe'
with open(os.environ['FIXTURE_CALLS'], 'a') as file:
    file.write(json.dumps(sys.argv[1:])+'\\n')
PY
    local tool="$1"; shift
    case "$tool" in
        lipo) echo arm64 ;;
        otool)
            if [[ "$1" == -l ]]; then printf 'path /usr/lib/swift\\npath @executable_path/../Frameworks\\n';
            else echo /usr/lib/libSystem.B.dylib; fi ;;
        xcrun)
            if [[ "$1" == --find ]]; then echo /fixture/swift-stdlib-tool;
            elif [[ "$1 $2" == 'notarytool submit' ]]; then
                if [[ "${{FIXTURE_SIGNAL:-}}" == yes ]]; then kill -TERM $$; fi
                if [[ "${{FIXTURE_FAILURE:-}}" == notary ]]; then echo '{{"status":"Invalid","id":"fixture"}}';
                else echo '{{"status":"Accepted","id":"fixture"}}'; fi
            elif [[ "$1 $2" == 'notarytool log' ]]; then
                printf '{{"status":"Accepted","issues":[]}}' > "${{@:$#}}";
            elif [[ "$1 $2" == 'stapler staple' && "${{@:$#}}" == *.app ]]; then
                printf fixture-ticket > "${{@:$#}}/Contents/CodeResources";
            fi ;;
        /fixture/swift-stdlib-tool) touch "${{@:$#}}/libswiftFixture.dylib" ;;
        base64) cat >/dev/null; printf fixture-certificate ;;
        security)
            if [[ "$1" == find-identity ]]; then echo '1) 1111111111111111111111111111111111111111 "Developer ID Application: Fixture (TEAM123456)"';
            elif [[ "$1" == create-keychain ]]; then touch "${{@:$#}}"; fi ;;
        codesign)
            if [[ "${{FIXTURE_FAILURE:-}}" == codesign && "$1" == --force ]]; then return 1; fi ;;
        spctl) ;;
        ditto)
            if [[ "$1" == -c ]]; then touch "${{@:$#}}";
            else cp -R "$1" "$2"; fi ;;
        hdiutil) touch "${{@:$#}}" ;;
        tar) command tar "$@" ;;
        *) echo 'unexpected native tool' >&2; return 1 ;;
    esac
}}
macos_release_main {shlex.quote(str(binary))} {shlex.quote(str(self.directory))} fixture {'--unsigned' if unsigned else ''}
""")
        env = dict(os.environ)
        env.update({
            "MAPLE_PACKAGE_CHANNEL": "dev", "MAPLE_PACKAGE_APP_NAME": self.info["display_name"],
            "MAPLE_PACKAGE_BUNDLE_ID": self.info["bundle_id"], "MAPLE_PACKAGE_BUILD_NUMBER": "42",
            "FIXTURE_BUILD_INFO": str(self.path), "FIXTURE_CALLS": str(log),
            "FIXTURE_FAILURE": failure, "FIXTURE_SIGNAL": "yes" if signal else "",
            "APPLE_CERTIFICATE": "fixture-cert", "APPLE_CERTIFICATE_PASSWORD": "fixture-cert-password",
            "APPLE_ID": "fixture-apple-id", "APPLE_ID_PASSWORD": "fixture-notary-password",
            "APPLE_TEAM_ID": "TEAM123456",
        })
        result = subprocess.run(["bash", str(wrapper)], env=env, capture_output=True, text=True)
        calls = [json.loads(line) for line in log.read_text().splitlines()] if log.exists() else []
        return result, calls

    def assert_secrets_not_returned(self, result):
        for value in ("fixture-cert-password", "fixture-notary-password", "fixture-cert"):
            self.assertNotIn(value, result.stdout + result.stderr)

    def test_inside_out_signing_notarization_and_ephemeral_keychain_cleanup(self):
        result, calls = self.run_packager()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assert_secrets_not_returned(result)
        signs = [call for call in calls if call[0] == "codesign" and "--force" in call]
        self.assertTrue(signs[0][-1].endswith("libswiftFixture.dylib"))
        self.assertTrue(signs[1][-1].endswith(".app"))
        self.assertIn("--options", signs[1])
        self.assertIn("runtime", signs[1])
        self.assertIn("--timestamp", signs[1])
        self.assertEqual(len([call for call in calls if call[:3] == ["xcrun", "notarytool", "submit"]]), 2)
        self.assertTrue(any(call[:2] == ["security", "delete-keychain"] for call in calls))
        self.assertFalse(any("default-keychain" in call or "list-keychains" in call for call in calls))
        self.assertFalse(list(self.directory.glob(".macos-release.*")))
        with tarfile.open(self.directory / "fixture.app.tar.gz") as archive:
            for member in archive:
                self.assertTrue(member.mode & 0o004, f"package content is not publicly readable: {member.name}")
                if member.isdir():
                    self.assertTrue(member.mode & 0o001, f"package directory is not publicly accessible: {member.name}")
        extracted = release_info.extract_app_archive(
            self.directory / "fixture.app.tar.gz", self.directory / "extracted", self.info["display_name"]
        )
        self.assertTrue((extracted / "Contents/MacOS/maple-agent").is_file())
        self.assertEqual(extracted.stat().st_mode & 0o777, 0o755)
        self.assertEqual((extracted / "Contents/Info.plist").stat().st_mode & 0o777, 0o644)

    def test_unsigned_pr_package_never_imports_credentials_or_notarizes(self):
        result, calls = self.run_packager(unsigned=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assert_secrets_not_returned(result)
        self.assertFalse(any(call[0] == "security" or "notarytool" in call for call in calls))
        self.assertTrue(all("--timestamp=none" in call for call in calls if call[0] == "codesign" and "--force" in call))

    def test_rejected_notary_response_and_signing_failure_cleanup(self):
        for failure in ("notary", "codesign"):
            with self.subTest(failure=failure):
                result, calls = self.run_packager(failure=failure)
                self.assertNotEqual(result.returncode, 0)
                self.assert_secrets_not_returned(result)
                self.assertTrue(any(call[:2] == ["security", "delete-keychain"] for call in calls))
                self.assertFalse(list(self.directory.glob(".macos-release.*")))

    def test_signal_cleans_owned_keychain_and_private_files(self):
        result, calls = self.run_packager(signal=True)
        self.assertEqual(result.returncode, 143)
        self.assert_secrets_not_returned(result)
        self.assertTrue(any(call[:2] == ["security", "delete-keychain"] for call in calls))
        self.assertFalse(list(self.directory.glob(".macos-release.*")))


if __name__ == "__main__":
    unittest.main()
