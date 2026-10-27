#!/usr/bin/env python3
"""Hermetic identity, artifact-integrity and native-signing boundary tests."""

import base64
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import plistlib
import shlex
import shutil
from signal import SIGKILL
import subprocess
import sys
import tempfile
import tarfile
import unittest
from test_macos_build_info import macho
from test_third_party_notices import notice_fixture


SCRIPTS = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("release_info", SCRIPTS / "release-info.py")
release_info = importlib.util.module_from_spec(spec)
spec.loader.exec_module(release_info)


def profile_info(profile):
    profiles = json.loads((SCRIPTS.parent / "release-profiles.json").read_text())
    return {field: profiles[profile][field] for field in release_info.PUBLIC_FIELDS}


class NativeArchiveMetadataTests(unittest.TestCase):
    @unittest.skipUnless(sys.platform == "darwin", "requires native macOS BSDtar metadata")
    def test_native_release_tar_omits_host_metadata_and_preserves_payload(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            app = root / "Maple Agent.app"
            executable = app / "Contents/MacOS/maple-agent"
            executable.parent.mkdir(parents=True)
            executable.write_bytes(b"public executable bytes\n")
            executable.chmod(0o755)
            ticket = app / "Contents/CodeResources"
            ticket.write_bytes(b"stapled ticket file bytes")
            ticket.chmod(0o644)
            signature = app / "Contents/_CodeSignature/CodeResources"
            signature.parent.mkdir()
            signature.write_bytes(b"signed resource manifest bytes")
            signature.chmod(0o644)
            link = app / "Contents/current"
            link.symlink_to("MacOS/maple-agent")
            subprocess.run(["/usr/bin/xattr", "-w", "com.maple.archive-fixture", "host-only metadata", str(executable)], check=True)
            subprocess.run(["/bin/chmod", "+a", "everyone allow read", str(executable)], check=True)
            subprocess.run(["/usr/bin/chflags", "hidden", str(executable)], check=True)
            self.assertIn("com.maple.archive-fixture", subprocess.check_output(["/usr/bin/xattr", str(executable)], text=True).splitlines())
            archive = root / "actual.app.tar.gz"
            result = subprocess.run([
                "/bin/bash", "-c", 'source "$1"; macos_archive_app "$2" "$3" "$4"',
                "native-release-archive-test", str(SCRIPTS / "macos-release-app.sh"),
                str(root), "Maple Agent", str(archive),
            ], check=True, capture_output=True, text=True)
            self.assertEqual(result.returncode, 0)
            with tarfile.open(archive) as contents:
                for member in contents:
                    self.assertFalse(any("xattr" in key.lower() or "acl" in key.lower() or "fflags" in key.lower() for key in member.pax_headers), member.pax_headers)
                    self.assertFalse(Path(member.name).name.startswith("._"), member.name)
            output = root / "roundtrip"
            output.mkdir()
            subprocess.run(["/usr/bin/tar", "-xf", str(archive), "-C", str(output)], check=True)
            extracted = output / app.name
            for original in (executable, ticket, signature):
                copy = extracted / original.relative_to(app)
                self.assertEqual(copy.read_bytes(), original.read_bytes())
                self.assertEqual(copy.stat().st_mode & 0o777, original.stat().st_mode & 0o777)
            self.assertEqual(os.readlink(extracted / link.relative_to(app)), "MacOS/maple-agent")
            self.assertNotIn("com.maple.archive-fixture", subprocess.check_output(["/usr/bin/xattr", str(extracted / executable.relative_to(app))], text=True).splitlines())


class NativeGatekeeperTests(unittest.TestCase):
    @unittest.skipUnless(sys.platform == "darwin", "requires native macOS Gatekeeper")
    def test_native_gatekeeper_resolves_without_path_lookup(self):
        with tempfile.TemporaryDirectory() as empty_path:
            result = subprocess.run(
                ["/bin/bash", "-c", 'source "$1"; macos_native spctl --status',
                 "native-gatekeeper-test", str(SCRIPTS / "macos-release-app.sh")],
                env={"PATH": empty_path}, capture_output=True, text=True,
            )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("assessments", result.stdout + result.stderr)


class PrebuiltReleaseTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.component = Path(self.temp.name)
        scripts = self.component / "scripts"
        scripts.mkdir()
        for name in ("verify-prebuilt-release.sh", "release-info.py", "macos-build-info.py"):
            shutil.copy2(SCRIPTS / name, scripts / name)
        shutil.copy2(SCRIPTS.parent / "release-profiles.json", self.component)
        self.env = dict(os.environ, GIT_CONFIG_GLOBAL=os.devnull, GIT_CONFIG_NOSYSTEM="1")
        self.git("init", "--quiet")
        self.git("add", ".")
        self.git("-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
                 "-c", "commit.gpgsign=false", "commit", "--quiet", "-m", "fixture source")
        self.source = self.git("rev-parse", "HEAD").stdout.strip()
        profile = profile_info("dev")
        self.info = {**profile, "profile": "dev", "version": "0.1.0",
                     "source_sha": self.source, "git_revision": self.source[:8]}
        self.metadata = self.component / "fixture-build-info.json"
        self.binary = self.component / "target/release/maple-agent"
        self.binary.parent.mkdir(parents=True)
        self.binary.write_bytes(macho(self.info))
        # Match permissions produced by actions/download-artifact.
        self.binary.chmod(0o644)
        self.scratch = self.component / "temporary-metadata"
        self.scratch.mkdir()
        tools = self.component / "tools"
        tools.mkdir()
        # Exercise the signing-runner branch on any fixture host. The artifact
        # is a valid metadata-only Mach-O; attempting to execute it must fail.
        (tools / "uname").write_text(f"#!{shutil.which('bash')}\necho Darwin\n")
        (tools / "uname").chmod(0o755)
        self.env["PATH"] = str(tools) + os.pathsep + self.env["PATH"]
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
        target = self.binary.resolve()
        target.write_bytes(macho(self.info))
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
        self.info = {**profile_info("dev"), "profile": "dev", "version": "0.1.0",
                     "source_sha": "a" * 40, "git_revision": "a" * 8}
        self.artifacts = self.root / "artifacts"
        self.artifacts.mkdir()
        (self.artifacts / "build-info.json").write_text(json.dumps(self.info) + "\n")
        self.launcher = f"""#!{sys.executable}
import sys
if sys.argv[1:] == ["--build-info"]:
    print({json.dumps(self.info)!r})
elif sys.argv[1:] == ["--version"]:
    print("Maple Agent fixture")
else:
    raise SystemExit("unexpected fixture launcher arguments")
"""
        self.archive = self.artifacts / release_info.linux_package_name(self.info)
        # Substitute only the audit/runtime closure and host identity; the real
        # verifier still checks the manifest, checksums, profile and source.
        (scripts / "linux-release-bundle.py").write_text("""import importlib.util
from pathlib import Path
import sys
if sys.argv[1] == "extract":
    assert sys.argv[4:] == ["--profile", "dev"]
    spec = importlib.util.spec_from_file_location("release_info", Path(__file__).with_name("release-info.py"))
    release_info = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(release_info)
    appdir = release_info.extract_app_archive(sys.argv[2], sys.argv[3], "maple-agent-dev")
else:
    assert sys.argv[1] == "audit", sys.argv
    appdir = Path(sys.argv[2])
assert appdir.stat().st_mode & 0o777 == 0o755
assert (appdir / "AppRun").stat().st_mode & 0o7777 == 0o755
assert (appdir / "resource.txt").stat().st_mode & 0o7777 == 0o644
private = appdir.parent / "private-after-extraction"
private.write_text("fixture private metadata")
assert private.stat().st_mode & 0o777 == 0o600, "extraction changed the parent's private umask"
assert appdir.parent.stat().st_mode & 0o777 == 0o700, "extraction parent must remain private"
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

    def write_archive(self):
        root = "maple-agent-dev.app"
        members = [(root, None, 0o755),
                   (root + "/AppRun", self.launcher, int(self.env["FIXTURE_EXEC_MODE"], 8)),
                   (root + "/resource.txt", "fixture data", int(self.env["FIXTURE_DATA_MODE"], 8))]
        with tarfile.open(self.archive, "w:gz") as archive:
            for name, content, mode in members:
                member = tarfile.TarInfo(name)
                member.mode = mode
                if content is None:
                    member.type = tarfile.DIRTYPE
                    archive.addfile(member)
                else:
                    data = content.encode()
                    member.size = len(data)
                    archive.addfile(member, io.BytesIO(data))
        release_info.write_manifest(self.info, self.artifacts, "linux-x86_64", True)

    def verify(self):
        self.write_archive()
        result = subprocess.run(["bash", str(self.component / "scripts/verify-release.sh"), "dev",
                                 str(self.artifacts), "--unsigned", "--platform", "linux-x86_64"],
                                env=self.env, capture_output=True, text=True)
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


class MacosUnsignedVerifierTests(unittest.TestCase):
    """Run the actual static verifier while substituting only native tools."""

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.component = self.root / "component"
        scripts = self.component / "scripts"
        scripts.mkdir(parents=True)
        for name in ("verify-release.sh", "release-info.py", "macos-build-info.py"):
            shutil.copy2(SCRIPTS / name, scripts / name)
        shutil.copy2(SCRIPTS.parent / "release-profiles.json", self.component)
        entitlements = self.component / "app/packaging/macos-entitlements.plist"
        entitlements.parent.mkdir(parents=True)
        shutil.copy2(SCRIPTS.parent / "app/packaging/macos-entitlements.plist", entitlements)
        self.info = {**profile_info("dev"), "profile": "dev", "version": "0.1.0",
                     "source_sha": "a" * 40, "git_revision": "a" * 8}
        self.bundle = self.root / f"{release_info.macos_bundle_name('dev')}.app"
        binary = self.bundle / "Contents/MacOS/maple-agent"
        binary.parent.mkdir(parents=True)
        # Metadata-only Mach-O cannot execute. A successful static verification
        # must read its section and signatures without launching artifact code.
        binary.write_bytes(macho(self.info))
        binary.chmod(0o755)
        release_info.write_plist(self.info, self.bundle / "Contents/Info.plist", "1")
        self.artifacts = self.root / "artifacts"
        self.artifacts.mkdir()
        (self.artifacts / "build-info.json").write_text(
            json.dumps(self.info, sort_keys=True, separators=(",", ":")) + "\n"
        )
        (self.artifacts / "fixture.dmg").write_text("fixture disk image")
        with tarfile.open(self.artifacts / "fixture.app.tar.gz", "w:gz") as archive:
            archive.add(self.bundle, arcname=self.bundle.name)
        release_info.write_manifest(self.info, self.artifacts, "macos-aarch64", True)
        self.wrapper = self.root / "verify-fixture.sh"
        self.wrapper.write_text("""#!/usr/bin/env bash
set -euo pipefail
uname() { if [[ "$1" == -s ]]; then echo Darwin; else echo arm64; fi; }
git() { printf '%040d\\n' 0 | tr 0 a; }
function /usr/bin/codesign {
    case "$1" in
        --verify) ;;
        --display)
            if [[ "$2" == --entitlements ]]; then
                cat "$FIXTURE_ENTITLEMENTS"
            else
                printf '%s\\n' "$FIXTURE_SIGNATURE"
            fi ;;
        *) echo 'unexpected codesign call' >&2; return 1 ;;
    esac
}
function /usr/bin/lipo { echo arm64; }
function /usr/bin/hdiutil {
    if [[ "$1" == attach ]]; then
        while [[ $# -gt 0 && "$1" != -mountpoint ]]; do shift; done
        [[ "$1" == -mountpoint ]]
        cp -R "$FIXTURE_BUNDLE" "$2/"
    elif [[ "$1" != detach ]]; then
        echo 'unexpected hdiutil call' >&2; return 1
    fi
}
source "$0" dev "$1" --unsigned --static
""")
        self.env = dict(os.environ, TMPDIR=str(self.root), FIXTURE_BUNDLE=str(self.bundle),
                        FIXTURE_ENTITLEMENTS=str(entitlements))

    def verify(self, directory_flags):
        self.env["FIXTURE_SIGNATURE"] = (
            "Executable=fixture\nIdentifier=cloud.opensecret.maple.agent.dev\n"
            + directory_flags + "\nSignature=adhoc\nTeamIdentifier=not set\n"
        )
        result = subprocess.run(
            ["bash", "-c", self.wrapper.read_text(), str(self.component / "scripts/verify-release.sh"), str(self.artifacts)],
            env=self.env, capture_output=True, text=True,
        )
        self.assertFalse(list(self.root.glob("maple-agent-verify.*")), "verifier private files must be cleaned")
        return result

    def test_ad_hoc_main_passes_real_static_artifact_verification(self):
        result = self.verify("CodeDirectory v=20400 size=512 flags=0x2(adhoc) hashes=8+5 location=embedded")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("verified dev macos-aarch64", result.stdout)

    def test_ad_hoc_main_rejects_hardened_runtime_library_validation_and_unknown_flags(self):
        for flags in ("0x10002(adhoc,runtime)", "0x2002(adhoc,library-validation)", "0x8002(adhoc)"):
            with self.subTest(flags=flags):
                result = self.verify(f"CodeDirectory v=20400 size=512 flags={flags} hashes=8+5 location=embedded")
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("unsigned macOS app must use only ad-hoc signing flags", result.stderr)
                self.assertNotIn("verified dev macos-aarch64", result.stdout)

    def test_ad_hoc_main_rejects_missing_or_malformed_flags(self):
        for directory in ("", "CodeDirectory v=20400 flags=unrecognized", "flags=0x2(adhoc)"):
            with self.subTest(directory=directory):
                result = self.verify(directory)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("unsigned macOS app must use only ad-hoc signing flags", result.stderr)


class ReleaseMetadataTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.directory = Path(self.temp.name)
        self.info = {
            **profile_info("dev"), "profile": "dev", "version": "0.1.0",
            "source_sha": "a" * 40, "git_revision": "a" * 8,
        }
        self.path = self.directory / "build-info.json"
        self.write_info()

    def write_info(self):
        self.path.write_text(json.dumps(self.info, sort_keys=True, separators=(",", ":")) + "\n")

    def test_profile_and_source_must_match_the_binary(self):
        release_info.read_info("dev", self.path, "a" * 40)
        for profile, source in (("prod", "a" * 40), ("dev", "b" * 40)):
            with self.subTest(profile=profile, source=source), self.assertRaises(ValueError):
                release_info.read_info(profile, self.path, source)

    def test_display_branding_preserves_installed_bundle_names_and_metadata_contract(self):
        for profile, display, bundle in (
            ("prod", "Maple Work", "Maple Agent"),
            ("dev", "Maple Work Dev", "Maple Agent Dev"),
        ):
            with self.subTest(profile=profile):
                info = {**self.info, **profile_info(profile), "profile": profile}
                self.path.write_text(json.dumps(info))
                result = subprocess.run(
                    [sys.executable, str(SCRIPTS / "release-info.py"), "bundle-name", profile, str(self.path)],
                    check=True, capture_output=True, text=True,
                )
                self.assertEqual(result.stdout.strip(), bundle)
                self.assertEqual(info["display_name"], display)
                output = self.directory / "Info.plist"
                release_info.write_plist(info, output, "42")
                plist = plistlib.loads(output.read_bytes())
                self.assertEqual(plist["CFBundleDisplayName"], bundle)
                self.assertEqual(plist["CFBundleName"], display)
                localized = output.parent / "Resources/en.lproj/InfoPlist.strings"
                self.assertEqual(plistlib.loads(localized.read_bytes()), {
                    "CFBundleDisplayName": display, "CFBundleName": display,
                })
                self.assertEqual(plist["CFBundleIdentifier"], info["bundle_id"])
                self.assertEqual(plist["CFBundleExecutable"], "maple-agent")
                self.assertEqual(set(info), release_info.BUILD_FIELDS)

    def test_dirty_or_incomplete_source_revision_is_rejected(self):
        for revision in ("aaaaaaaa-dirty", "unknown", "bbbbbbbb"):
            self.info["git_revision"] = revision
            self.write_info()
            with self.subTest(revision=revision), self.assertRaises(ValueError):
                release_info.read_info("dev", self.path)

    def test_debug_bundle_localizes_branding_without_changing_its_identity(self):
        output = self.directory / "Maple Agent Debug.app/Contents/Info.plist"
        output.parent.mkdir(parents=True)
        shutil.copyfile(SCRIPTS.parent / "app/macos/Info.plist", output)
        subprocess.run(
            [sys.executable, str(SCRIPTS / "macos-debug-plist.py"), str(output)],
            env=dict(os.environ, MAPLE_DEBUG_BUNDLE_ID="cloud.opensecret.maple.agent.debug"),
            check=True, capture_output=True, text=True,
        )
        plist = plistlib.loads(output.read_bytes())
        self.assertEqual(plist["CFBundleIdentifier"], "cloud.opensecret.maple.agent.debug")
        self.assertEqual(plist["CFBundleExecutable"], "maple-agent")
        self.assertEqual(plist["CFBundleDisplayName"], "Maple Agent Debug")
        localized = output.parent / "Resources/en.lproj/InfoPlist.strings"
        self.assertEqual(plistlib.loads(localized.read_bytes()), {
            "CFBundleDisplayName": "Maple Work Debug", "CFBundleName": "Maple Work Debug",
        })

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

    def test_update_feed_and_keys_cannot_drift_from_the_packaged_profile(self):
        for field, value in (
            ("update_feed_url", "https://wrong.invalid/agent/stable/latest.json"),
            ("update_public_keys", ["attacker-key"]),
        ):
            original = self.info[field]
            self.info[field] = value
            self.write_info()
            with self.subTest(field=field), self.assertRaises(ValueError):
                release_info.read_info("dev", self.path)
            self.info[field] = original

    def test_prod_metadata_requires_the_configured_primary_and_backup_pins(self):
        prod = profile_info("prod")
        self.info = {**prod, "profile": "prod", "version": "0.0.1",
                     "source_sha": "a" * 40, "git_revision": "a" * 8}
        self.write_info()
        self.assertEqual(len(release_info.read_info("prod", self.path)["update_public_keys"]), 2)
        for keys in ([], prod["update_public_keys"][:1],
                     list(reversed(prod["update_public_keys"]))):
            self.info["update_public_keys"] = keys
            self.write_info()
            with self.subTest(keys=keys), self.assertRaisesRegex(ValueError, "release profile"):
                release_info.read_info("prod", self.path)

    def test_prod_feed_validation_uses_the_explicit_platform_not_the_input_url(self):
        profiles = json.loads((SCRIPTS.parent / "release-profiles.json").read_text())
        self.info = {**profile_info("prod"), "profile": "prod", "version": "0.1.0",
                     "source_sha": "a" * 40, "git_revision": "a" * 8}
        feeds = {
            None: profiles["prod"]["update_feed_url"],
            "macos-aarch64": profiles["prod"]["update_feed_url"],
            "linux-x86_64": profiles["prod"]["linux_update_feed_url"],
            "windows-x86_64": profiles["prod"]["windows_update_feed_url"],
        }
        for platform, expected in feeds.items():
            for feed in {*feeds.values(), None, "https://wrong.invalid/agent/stable/latest.json"}:
                with self.subTest(platform=platform, feed=feed):
                    self.info["update_feed_url"] = feed
                    self.write_info()
                    if feed == expected:
                        release_info.read_info("prod", self.path, platform=platform)
                    else:
                        with self.assertRaisesRegex(ValueError, "update_feed_url"):
                            release_info.read_info("prod", self.path, platform=platform)

    def test_backup_requires_a_distinct_public_key_not_a_changed_comment_or_id(self):
        def key(point, key_id=b"12345678", comment="fixture"):
            packet = base64.b64encode(b"Ed" + key_id + point).decode()
            return f"untrusted comment: {comment}\n{packet}\n"
        primary = key(bytes(range(32)))
        release_info.validate_update_keys([])
        release_info.validate_update_keys([primary, key(bytes(reversed(range(32))))])
        for duplicate in [primary, key(bytes(range(32)), comment="backup"),
                          key(bytes(range(32)), key_id=b"87654321")]:
            with self.subTest(duplicate=duplicate), self.assertRaisesRegex(ValueError, "distinct"):
                release_info.validate_update_keys([primary, duplicate])

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

    def test_macos_plist_cli_preserves_its_default_build_number(self):
        output = self.directory / "Info.plist"
        for arguments, expected in (([], "1"), (["--build-number", "42"], "42")):
            with self.subTest(arguments=arguments):
                subprocess.run(
                    [sys.executable, str(SCRIPTS / "release-info.py"), "plist", "dev", str(self.path),
                     "--output", str(output), *arguments], check=True, capture_output=True, text=True,
                )
                self.assertEqual(plistlib.loads(output.read_bytes())["CFBundleVersion"], expected)

    def test_macos_plist_cli_rejects_an_explicitly_empty_build_number(self):
        output = self.directory / "Info.plist"
        result = subprocess.run(
            [sys.executable, str(SCRIPTS / "release-info.py"), "plist", "dev", str(self.path),
             "--output", str(output), "--build-number", ""], capture_output=True, text=True,
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("package build number must be a positive integer", result.stderr)
        self.assertFalse(output.exists())
        self.assertFalse((self.directory / "Resources").exists())

    def package(self, unsigned=True):
        (self.directory / "fixture.dmg").write_bytes(b"package")
        release_info.write_manifest(self.info, self.directory, "macos-aarch64", unsigned)

    def test_unsigned_package_cannot_pass_master_verification(self):
        self.package()
        release_info.verify_artifacts("dev", self.directory, True)
        with self.assertRaises(ValueError):
            release_info.verify_artifacts("dev", self.directory, False)

    def test_macos_manifest_keeps_its_existing_schema_without_windows_fields(self):
        self.package(False)
        manifest = release_info.verify_artifacts("dev", self.directory, False, platform="macos-aarch64")
        self.assertEqual(set(manifest), {
            "schema_version", "profile", "platform", "source_sha", "version",
            "package", "distribution", "macos_signing",
        })
        self.assertEqual(manifest["macos_signing"], "developer-id-notarized")

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


class LinuxReleaseMetadataTests(unittest.TestCase):
    setUp = ReleaseMetadataTests.setUp
    write_info = ReleaseMetadataTests.write_info

    def package(self):
        package = self.directory / release_info.linux_package_name(self.info)
        package.write_bytes(b"compressed Linux bundle fixture")
        release_info.write_manifest(self.info, self.directory, "linux-x86_64", True)
        return package

    def write_checksums(self):
        (self.directory / "SHA256SUMS").write_text("".join(
            f"{hashlib.sha256(path.read_bytes()).hexdigest()}  {path.name}\n"
            for path in sorted(self.directory.iterdir()) if path.name != "SHA256SUMS"
        ))

    def test_linux_package_records_checksum_policy_and_exact_public_inventory(self):
        package = self.package()
        manifest = release_info.verify_artifacts("dev", self.directory, True, platform="linux-x86_64")
        self.assertEqual(manifest["schema_version"], 1)
        self.assertEqual(manifest["package"], "maple-agent-dev-0.1.0-aaaaaaaa-linux-x86_64.tar.gz")
        self.assertIsNone(manifest["macos_signing"])
        self.assertEqual(manifest["linux_integrity"], "sha256")
        self.assertEqual(set(manifest), {
            "schema_version", "profile", "platform", "source_sha", "version",
            "package", "distribution", "macos_signing", "linux_integrity",
        })
        self.assertEqual({path.name for path in self.directory.iterdir()},
                         {"build-info.json", "manifest.json", "SHA256SUMS", package.name})
        self.assertTrue(all(path.stat().st_mode & 0o777 == 0o644 for path in self.directory.iterdir()))
        with self.assertRaisesRegex(ValueError, "requested platform"):
            release_info.verify_artifacts("dev", self.directory, True, platform="macos-aarch64")

    def test_linux_package_name_must_match_profile_version_and_source_revision(self):
        expected = release_info.linux_package_name(self.info)
        for name in ("fixture.AppImage", "fixture.tar.gz", expected.replace("-dev-", "-prod-"),
                     expected.replace("-0.1.0-", "-0.2.0-"), expected.replace("-aaaaaaaa-", "-bbbbbbbb-")):
            with self.subTest(name=name):
                package = self.directory / name
                package.write_bytes(b"wrong package")
                with self.assertRaises(ValueError):
                    release_info.write_manifest(self.info, self.directory, "linux-x86_64", True)
                package.unlink()

    def test_linux_extra_files_fail_even_with_valid_checksums(self):
        self.package()
        extra = self.directory / "extra.txt"
        extra.write_text("checked but not public release inventory")
        self.write_checksums()
        with self.assertRaisesRegex(ValueError, "exactly the expected bundle"):
            release_info.verify_artifacts("dev", self.directory, True)
        with self.assertRaises(ValueError):
            release_info.write_manifest(self.info, self.directory, "linux-x86_64", True)

    def test_downloaded_renamed_linux_package_is_rejected_with_consistent_manifest_and_checksums(self):
        package = self.package()
        package.rename(self.directory / "fixture.tar.gz")
        path = self.directory / "manifest.json"
        manifest = json.loads(path.read_text())
        path.write_text(json.dumps({**manifest, "package": "fixture.tar.gz"}))
        self.write_checksums()
        with self.assertRaisesRegex(ValueError, "exactly the expected bundle"):
            release_info.verify_artifacts("dev", self.directory, True)

    def test_linux_prod_artifact_validates_the_platform_specific_feed(self):
        profiles = json.loads((SCRIPTS.parent / "release-profiles.json").read_text())
        self.info = {**profile_info("prod"), "profile": "prod", "version": "0.1.0",
                     "source_sha": "a" * 40, "git_revision": "a" * 8,
                     "update_feed_url": profiles["prod"]["linux_update_feed_url"]}
        self.write_info()
        self.package()
        release_info.verify_artifacts("prod", self.directory, True, platform="linux-x86_64")
        self.info["update_feed_url"] = profiles["prod"]["update_feed_url"]
        self.write_info()
        self.write_checksums()
        with self.assertRaisesRegex(ValueError, "update_feed_url"):
            release_info.verify_artifacts("prod", self.directory, True, platform="linux-x86_64")

    def test_linux_integrity_and_signing_policy_cannot_be_relabelled(self):
        self.package()
        path = self.directory / "manifest.json"
        original = json.loads(path.read_text())
        for field, value in (("macos_signing", "developer-id-notarized"),
                             ("linux_integrity", "signed"), ("linux_integrity", None)):
            with self.subTest(field=field, value=value):
                path.write_text(json.dumps({**original, field: value}))
                self.write_checksums()
                with self.assertRaisesRegex(ValueError, "Linux integrity"):
                    release_info.verify_artifacts("dev", self.directory, True)
        path.write_text(json.dumps({key: value for key, value in original.items() if key != "macos_signing"}))
        self.write_checksums()
        with self.assertRaisesRegex(ValueError, "Linux integrity"):
            release_info.verify_artifacts("dev", self.directory, True)

    def test_linux_symlink_or_directory_cannot_replace_public_metadata(self):
        self.package()
        for name in ("build-info.json", "manifest.json", "SHA256SUMS"):
            with self.subTest(name=name):
                path = self.directory / name
                original = path.read_bytes()
                path.unlink()
                path.symlink_to(self.directory / "outside")
                with self.assertRaisesRegex(ValueError, "symlink"):
                    release_info.verify_artifacts("dev", self.directory, True)
                path.unlink()
                path.write_bytes(original)
        (self.directory / "unexpected-directory").mkdir()
        with self.assertRaisesRegex(ValueError, "unsupported file"):
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

    def test_profile_extraction_keeps_the_installed_name_after_rebranding(self):
        self.write_archive()
        output = self.directory / "profile-output"
        subprocess.run(
            [sys.executable, str(SCRIPTS / "release-info.py"), "extract-app", "dev",
             str(self.archive), "--output", str(output)],
            check=True, capture_output=True, text=True,
        )
        self.assertTrue((output / "Maple Agent Dev.app/Contents/maple-agent").is_file())
        self.assertFalse((output / "Maple Work Dev.app").exists())

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
    search_list = [
        "/Users/Fixture User/Library/Keychains/login.keychain-db",
        '/Users/Fixture User/Library/Keychains/Quoted "Name".keychain-db',
        r"/Users/Fixture User/Library/Keychains/Back\slash.keychain-db",
        "/Library/Keychains/System.keychain",
    ]

    def run_packager(self, unsigned=False, failure="", signal=False, search_list=None):
        component = self.directory / "fixture-component"
        notice_fixture(component, "aarch64-apple-darwin")
        # Nix sources are read-only. Copy bytes into this mutable fixture rather
        # than preserving modes that would prevent a second packaging attempt.
        for name in ("macos-release-app.sh", "release-info.py", "macos-build-info.py"):
            shutil.copyfile(SCRIPTS / name, component / "scripts" / name)
        shutil.copyfile(SCRIPTS.parent / "release-profiles.json", component / "release-profiles.json")
        (component / "app/packaging").mkdir(exist_ok=True)
        (component / "app/macos").mkdir(exist_ok=True)
        shutil.copyfile(SCRIPTS.parent / "app/macos/Info.plist", component / "app/macos/Info.plist")
        shutil.copyfile(SCRIPTS.parent / "app/packaging/macos-entitlements.plist", component / "app/packaging/macos-entitlements.plist")
        for icon in ("maple-agent.icns", "maple-agent-dev.icns"):
            shutil.copyfile(SCRIPTS.parent / "app/packaging" / icon, component / "app/packaging" / icon)
        binary = self.directory / "maple-agent"
        # Static-only fixture cannot be launched. Successful signing packaging
        # proves the keychain-bearing path never executes artifact code.
        binary.write_bytes(macho(self.path.read_bytes()))
        binary.chmod(0o755)
        log = self.directory / "native-calls.jsonl"
        log.unlink(missing_ok=True)
        search_state = self.directory / "keychain-search.json"
        search_state.write_text(json.dumps(self.search_list if search_list is None else search_list))
        # Bash 5.3 uses a pipe for this heredoc, but the Darwin pipe capacity
        # can be smaller than its threshold. Keep this longer mock as a file
        # so a command substitution cannot block before Python starts.
        security_list = self.directory / "mock-security-list.py"
        security_list.write_text("""import json,os,sys
from pathlib import Path
state=Path(os.environ['FIXTURE_KEYCHAIN_SEARCH'])
args=sys.argv[1:]
if args == ['list-keychains','-d','user']:
    if os.environ.get('FIXTURE_FAILURE') == 'malformed-search':
        print('not a quoted path')
    else:
        for path in json.loads(state.read_text()):
            print('    "'+path+'"')
else:
    assert args[:4] == ['list-keychains','-d','user','-s'], args
    paths=args[4:]
    adding=any('/.macos-release.' in path and path.endswith('/signing.keychain-db') for path in paths)
    failure=os.environ.get('FIXTURE_FAILURE')
    if failure == 'restore-search' and not adding:
        raise SystemExit(1)
    state.write_text(json.dumps(paths))
    # Simulate a setter that changed state before reporting failure.
    if failure == 'set-search' and adding:
        raise SystemExit(1)
""")
        wrapper = self.directory / "mock-packager.sh"
        wrapper.write_text(f"""#!/usr/bin/env bash
set -euo pipefail
source {shlex.quote(str(component / 'scripts/macos-release-app.sh'))}
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
            elif [[ "$1" == create-keychain ]]; then touch "${{@:$#}}";
            elif [[ "$1" == list-keychains ]]; then
                python3 {shlex.quote(str(security_list))} "$@"
            fi ;;
        codesign)
            if [[ "$1" == --force && "$3" != - ]]; then
                python3 - "$@" <<'PY'
import json,os,sys
from pathlib import Path
args=sys.argv[1:]
assert args[args.index('--sign')+1] == '1111111111111111111111111111111111111111'
assert '--keychain' in args, 'signing lookup must remain scoped to its keychain'
keychain=args[args.index('--keychain')+1]
assert keychain in json.loads(Path(os.environ['FIXTURE_KEYCHAIN_SEARCH']).read_text()), 'no identity found: owned keychain missing from search list'
PY
            fi
            if [[ "${{FIXTURE_FAILURE:-}}" == codesign && "$1" == --force ]]; then return 1; fi ;;
        spctl) ;;
        ditto)
            if [[ "$1" == -c ]]; then touch "${{@:$#}}";
            else cp -R "$1" "$2"; fi ;;
        hdiutil)
            if [[ "$1" == create && "${{FIXTURE_FAILURE:-}}" == hdiutil ]]; then
                echo 'hdiutil: create failed - No space left on device' >&2; return 1
            fi
            touch "${{@:$#}}" ;;
        /bin/df) echo 'Filesystem Size Used Avail Capacity Mounted on' ;;
        tar)
            # The mocked native boundary may run on Linux; actual BSDtar
            # metadata behavior has a separate native macOS regression.
            local args=() argument
            for argument in "$@"; do
                case "$argument" in --no-xattrs|--no-fflags|--no-acls|--no-mac-metadata) ;;
                    *) args+=("$argument") ;; esac
            done
            command tar "${{args[@]}}" ;;
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
            "FIXTURE_KEYCHAIN_SEARCH": str(search_state),
            "FIXTURE_FAILURE": failure, "FIXTURE_SIGNAL": "yes" if signal else "",
            "APPLE_CERTIFICATE": "fixture-cert", "APPLE_CERTIFICATE_PASSWORD": "fixture-cert-password",
            "APPLE_ID": "fixture-apple-id", "APPLE_ID_PASSWORD": "fixture-notary-password",
            "APPLE_TEAM_ID": "TEAM123456",
        })
        process = subprocess.Popen(["bash", str(wrapper)], env=env, stdout=subprocess.PIPE,
                                   stderr=subprocess.PIPE, text=True, start_new_session=True)
        try:
            stdout, stderr = process.communicate(timeout=60)
        except subprocess.TimeoutExpired:
            # The fixture owns its process group, including nested mock-tool
            # children. Reap all of them when it times out, not unrelated tests.
            try:
                os.killpg(process.pid, SIGKILL)
            except ProcessLookupError:
                pass
            process.communicate()
            self.fail("mock macOS packager exceeded its 60-second timeout")
        result = subprocess.CompletedProcess(process.args, process.returncode, stdout, stderr)
        calls = [json.loads(line) for line in log.read_text().splitlines()] if log.exists() else []
        return result, calls

    def assert_secrets_not_returned(self, result):
        for value in ("fixture-cert-password", "fixture-notary-password", "fixture-cert"):
            self.assertNotIn(value, result.stdout + result.stderr)

    def assert_search_list_restored(self, calls, expected=None):
        expected = self.search_list if expected is None else expected
        self.assertEqual(json.loads((self.directory / "keychain-search.json").read_text()), expected)
        setters = [call for call in calls if call[:4] == ["security", "list-keychains", "-d", "user"] and "-s" in call]
        self.assertEqual(setters[-1][5:], expected)
        self.assertLess(calls.index(setters[-1]), next(i for i, call in enumerate(calls) if call[:2] == ["security", "delete-keychain"]))

    def test_inside_out_signing_notarization_and_ephemeral_keychain_cleanup(self):
        result, calls = self.run_packager()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assert_secrets_not_returned(result)
        archive = next(call for call in calls if call[0] == "tar")
        self.assertEqual(archive[1:5], ["--no-xattrs", "--no-fflags", "--no-acls", "--no-mac-metadata"])
        self.assertEqual(archive[-1], "Maple Agent Dev.app")
        image = next(call for call in calls if call[:2] == ["hdiutil", "create"])
        self.assertEqual(image[image.index("-volname") + 1], "Maple Work Dev")
        signs = [call for call in calls if call[0] == "codesign" and "--force" in call]
        self.assertTrue(signs[0][-1].endswith("libswiftFixture.dylib"))
        self.assertTrue(signs[1][-1].endswith(".app"))
        self.assertIn("--options", signs[1])
        self.assertEqual(signs[1][signs[1].index("--options") + 1], "runtime")
        self.assertIn("--timestamp", signs[1])
        self.assertEqual(len([call for call in calls if call[:3] == ["xcrun", "notarytool", "submit"]]), 2)
        self.assertTrue(any(call[:2] == ["security", "delete-keychain"] for call in calls))
        settings = [call for call in calls if call[:2] == ["security", "set-keychain-settings"]]
        self.assertEqual(len(settings), 1)
        self.assertEqual(len(settings[0]), 3, "owned keychain must not lock during notarization")
        self.assertFalse(any("default-keychain" in call for call in calls))
        query = ["security", "list-keychains", "-d", "user"]
        create = next(i for i, call in enumerate(calls) if call[:2] == ["security", "create-keychain"])
        self.assertLess(calls.index(query), create, "snapshot must precede keychain creation")
        setters = [call for call in calls if call[:5] == query + ["-s"]]
        self.assertEqual(len(setters), 2)
        self.assertEqual(setters[0][5:-1], self.search_list, "existing paths and order must be preserved")
        self.assertTrue(setters[0][-1].endswith("/signing.keychain-db"))
        self.assert_search_list_restored(calls)
        self.assertFalse(list(self.directory.glob(".macos-release.*")))
        with tarfile.open(self.directory / "fixture.app.tar.gz") as archive:
            bundle = release_info.macos_bundle_name("dev") + ".app"
            plist = plistlib.loads(archive.extractfile(f"{bundle}/Contents/Info.plist").read())
            self.assertEqual(plist["CFBundleDisplayName"], "Maple Agent Dev")
            localized = plistlib.loads(archive.extractfile(f"{bundle}/Contents/Resources/en.lproj/InfoPlist.strings").read())
            self.assertEqual(localized["CFBundleDisplayName"], "Maple Work Dev")
            icon = archive.extractfile(f"{bundle}/Contents/Resources/MapleAgent.icns").read()
            self.assertEqual(icon, (SCRIPTS.parent / "app/packaging/maple-agent-dev.icns").read_bytes())
            self.assertNotEqual(icon, (SCRIPTS.parent / "app/packaging/maple-agent.icns").read_bytes())
            for member in archive:
                self.assertTrue(member.mode & 0o004, f"package content is not publicly readable: {member.name}")
                if member.isdir():
                    self.assertTrue(member.mode & 0o001, f"package directory is not publicly accessible: {member.name}")
        extracted = release_info.extract_app_archive(
            self.directory / "fixture.app.tar.gz", self.directory / "extracted", release_info.macos_bundle_name("dev")
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
        main_sign = next(call for call in calls if call[:2] == ["codesign", "--force"] and call[-1].endswith(".app"))
        self.assertEqual(main_sign[main_sign.index("--sign") + 1], "-")
        self.assertEqual(main_sign[main_sign.index("--options") + 1], "0")
        self.assertEqual(main_sign[main_sign.index("--entitlements") + 1],
                         str(self.directory / "fixture-component/app/packaging/macos-entitlements.plist"))

    def test_rejected_notary_response_and_signing_failure_cleanup(self):
        for failure in ("notary", "codesign"):
            with self.subTest(failure=failure):
                result, calls = self.run_packager(failure=failure)
                self.assertNotEqual(result.returncode, 0)
                self.assert_secrets_not_returned(result)
                self.assertTrue(any(call[:2] == ["security", "delete-keychain"] for call in calls))
                self.assert_search_list_restored(calls)
                self.assertFalse(list(self.directory.glob(".macos-release.*")))

    def test_signal_cleans_owned_keychain_and_private_files(self):
        result, calls = self.run_packager(signal=True)
        self.assertEqual(result.returncode, 143)
        self.assert_secrets_not_returned(result)
        self.assertTrue(any(call[:2] == ["security", "delete-keychain"] for call in calls))
        self.assert_search_list_restored(calls)
        self.assertFalse(list(self.directory.glob(".macos-release.*")))

    def test_failed_search_list_mutation_still_restores_before_deleting_keychain(self):
        result, calls = self.run_packager(failure="set-search")
        self.assertNotEqual(result.returncode, 0)
        self.assert_secrets_not_returned(result)
        self.assert_search_list_restored(calls)
        self.assertFalse(any(call[0] == "codesign" for call in calls))
        self.assertFalse(list(self.directory.glob(".macos-release.*")))

    def test_empty_original_search_list_is_restored(self):
        result, calls = self.run_packager(search_list=[])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assert_search_list_restored(calls, expected=[])

    def test_malformed_search_list_fails_before_keychain_setup(self):
        result, calls = self.run_packager(failure="malformed-search")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("invalid quoted macOS keychain search-list path", result.stderr)
        self.assertFalse(any(call[:2] == ["security", "create-keychain"] for call in calls))
        self.assertFalse(any(call[0] == "codesign" or "-s" in call for call in calls))
        self.assertEqual(json.loads((self.directory / "keychain-search.json").read_text()), self.search_list)

    def test_disk_image_is_sized_explicitly_instead_of_estimated(self):
        for unsigned in (False, True):
            with self.subTest(unsigned=unsigned):
                result, calls = self.run_packager(unsigned=unsigned)
                self.assertEqual(result.returncode, 0, result.stderr)
                create = next(call for call in calls if call[:2] == ["hdiutil", "create"])
                self.assertEqual(create[create.index("-fs") + 1], "HFS+")
                size = create[create.index("-size") + 1]
                self.assertRegex(size, r"^[0-9]+m$")
                self.assertGreaterEqual(int(size[:-1]), 65, "image needs the payload plus fixed headroom")

    def test_disk_image_failure_reports_free_space_and_still_cleans_up(self):
        result, calls = self.run_packager(failure="hdiutil")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("hdiutil could not create a", result.stderr)
        self.assertTrue(any(call[:2] == ["/bin/df", "-h"] for call in calls))
        self.assert_secrets_not_returned(result)
        self.assertTrue(any(call[:2] == ["security", "delete-keychain"] for call in calls))
        self.assert_search_list_restored(calls)
        self.assertFalse(list(self.directory.glob(".macos-release.*")))
        self.assertFalse((self.directory / "fixture.app.tar.gz").exists())

    def test_restore_failure_is_visible_and_fails_otherwise_successful_packaging(self):
        result, calls = self.run_packager(failure="restore-search")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("failed to restore the original macOS keychain search list", result.stderr)
        self.assert_secrets_not_returned(result)
        self.assertTrue(any(call[:2] == ["security", "delete-keychain"] for call in calls))
        self.assertFalse(list(self.directory.glob(".macos-release.*")))


if __name__ == "__main__":
    unittest.main()
