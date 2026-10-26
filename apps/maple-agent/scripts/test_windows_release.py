#!/usr/bin/env python3
"""Windows package identity, extraction and signing-boundary regressions."""

import hashlib
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch
import zipfile


SCRIPTS = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("windows_release", SCRIPTS / "windows-release.py")
release = importlib.util.module_from_spec(spec)
spec.loader.exec_module(release)


def info(profile="dev"):
    profiles = json.loads((SCRIPTS.parent / "release-profiles.json").read_text())
    return {
        **{key: profiles[profile][key] for key in release.release_info.PUBLIC_FIELDS},
        "profile": profile, "version": "0.1.2", "source_sha": "a" * 40,
        "git_revision": "a" * 8, "update_feed_url": None,
    }


class WindowsMetadataTests(unittest.TestCase):
    def test_windows_prod_has_its_own_disabled_feed_without_erasing_public_keys(self):
        identity = info("prod")
        release.release_info.validate_info("prod", identity, "a" * 40, release.PLATFORM)
        self.assertEqual(len(identity["update_public_keys"]), 2)
        for url in ("https://updates.maple.ai/agent/stable/latest.json", "https://updates.maple.ai/agent/stable/linux/latest.json"):
            with self.subTest(url=url), self.assertRaises(ValueError):
                release.release_info.validate_info("prod", {**identity, "update_feed_url": url}, "a" * 40, release.PLATFORM)

    def test_windows_manifest_binds_exact_installer_name_source_profile_and_signing_policy(self):
        for profile in ("dev", "prod"):
            for unsigned in (False, True):
                with self.subTest(profile=profile, unsigned=unsigned), tempfile.TemporaryDirectory() as temporary:
                    directory = Path(temporary)
                    identity = info(profile)
                    package = directory / release.release_info.windows_package_name(identity)
                    package.write_bytes(b"installer fixture")
                    (directory / "build-info.json").write_text(json.dumps(identity))
                    release.release_info.write_manifest(identity, directory, release.PLATFORM, unsigned)
                    manifest = release.release_info.verify_artifacts(profile, directory, unsigned, "a" * 40, release.PLATFORM)
                    self.assertEqual(manifest["windows_signing"], "unsigned" if unsigned else "authenticode")
                    with self.assertRaises(ValueError):
                        release.release_info.verify_artifacts(profile, directory, not unsigned, "a" * 40, release.PLATFORM)
                    with self.assertRaises(ValueError):
                        release.release_info.verify_artifacts(profile, directory, unsigned, "b" * 40, release.PLATFORM)
                    package.rename(directory / "renamed-setup.exe")
                    with self.assertRaises(ValueError):
                        release.release_info.write_manifest(identity, directory, release.PLATFORM, unsigned)

    def test_unknown_installer_payload_cannot_be_blessed_by_new_checksums(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            identity = info()
            (directory / release.release_info.windows_package_name(identity)).write_bytes(b"installer fixture")
            (directory / "build-info.json").write_text(json.dumps(identity))
            (directory / "extra.exe").write_bytes(b"unexpected")
            with self.assertRaises(ValueError):
                release.release_info.write_manifest(identity, directory, release.PLATFORM, True)


class RuntimeClosureTests(unittest.TestCase):
    def test_os_components_and_api_sets_are_allowed_but_compiler_runtime_is_not(self):
        allowed = ["kernel32.dll", "dwrite.dll", "d3d11.dll", "uiautomationcore.dll",
                   "api-ms-win-core-synch-l1-2-0.dll"]
        with patch.object(release.pe, "inspect_pe", return_value={"imports": allowed}):
            release.validate_imports(Path("unused.exe"))
        for unexpected in ("vcruntime140.dll", "msvcp140.dll", "libcrypto.dll", "onnxruntime.dll", "api-ms-win-../../evil.dll", "unknown.dll"):
            with self.subTest(unexpected=unexpected), patch.object(release.pe, "inspect_pe", return_value={"imports": allowed + [unexpected]}):
                with self.assertRaisesRegex(ValueError, "non-OS DLL"):
                    release.validate_imports(Path("unused.exe"))


class InstallerExtractionTests(unittest.TestCase):
    def inventory(self, names):
        return "\n\n".join(f"Path = {name}\nSize = 17\nFolder = -" for name in names) + "\n"

    def extract(self, directory, names, unsigned):
        def command(arguments, **_options):
            if arguments[1] == "l":
                return subprocess.CompletedProcess(arguments, 0, self.inventory(names))
            return subprocess.CompletedProcess(arguments, 0)
        def member(_extractor, _installer, _name, destination, _limit):
            destination.parent.mkdir(parents=True, exist_ok=True)
            destination.write_bytes(b"fixture content!!")
            return 17
        with patch.object(release, "seven_zip", return_value="7z"), patch.object(release, "run", side_effect=command), patch.object(release, "extract_member", side_effect=member):
            return release.extract_installer(Path("fixture-setup.exe"), directory, unsigned)

    def test_exact_signed_and_unsigned_payload_inventories(self):
        for unsigned, uninstaller in ((False, "uninstall.exe"), (True, "uninstall.exe"), (True, "uninstall.exe.nsis")):
            with self.subTest(unsigned=unsigned), tempfile.TemporaryDirectory() as temporary:
                directory = Path(temporary)
                result = self.extract(directory, sorted(release.PAYLOAD_FILES | {uninstaller}), unsigned)
                self.assertEqual(result, directory / uninstaller)

    def test_rejects_traversal_duplicate_extra_and_missing_before_extraction(self):
        valid = sorted(release.PAYLOAD_FILES | {"uninstall.exe.nsis"})
        for names in (valid + ["../escape"], valid + ["C:/escape"], valid + ["extra.dll"], valid + [valid[0]], valid[:-1]):
            with self.subTest(names=names), tempfile.TemporaryDirectory() as temporary:
                directory = Path(temporary)
                with self.assertRaises(ValueError):
                    self.extract(directory, names, True)
                self.assertEqual(list(directory.iterdir()), [])

    def test_reconstructed_uninstaller_can_omit_preliminary_size(self):
        names = sorted(release.PAYLOAD_FILES | {"uninstall.exe"})
        listing = self.inventory(names).replace("Path = uninstall.exe\nSize = 17", "Path = uninstall.exe\nSize = ")
        with tempfile.TemporaryDirectory() as temporary, patch.object(self, "inventory", return_value=listing):
            self.assertEqual(self.extract(Path(temporary), names, True).name, "uninstall.exe")

    def test_member_stream_cannot_write_beyond_its_budget(self):
        original = subprocess.Popen
        def fixture(*_arguments, **options):
            return original([sys.executable, "-c", "import sys; sys.stdout.buffer.write(b'x' * 65536)"], **options)
        with tempfile.TemporaryDirectory() as temporary, patch.object(subprocess, "Popen", side_effect=fixture):
            destination = Path(temporary) / "payload"
            with self.assertRaisesRegex(ValueError, "extraction bounds"):
                release.extract_member("fixture", Path("unused.exe"), "uninstall.exe", destination, 1024)
            self.assertLessEqual(destination.stat().st_size, 1024)


class PackageBoundaryTests(unittest.TestCase):
    def test_packager_signs_payload_before_compression_and_final_output_afterward_without_running_app(self):
        with tempfile.TemporaryDirectory() as temporary:
            component = Path(temporary)
            binary = component / "target/release/maple-agent.exe"
            binary.parent.mkdir(parents=True)
            binary.write_bytes(b"downloaded executable must not run")
            (component / "LICENSE").write_text("license fixture")
            calls = []
            def stage(_component, directory, _target):
                directory.mkdir(parents=True)
            def sign(file, unsigned, *, sign=False):
                calls.append(("sign" if sign else "verify-signature", Path(file).name))
            def compiler(arguments, **_options):
                calls.append(("compiler", str(arguments[0])))
                output = next((component / "dist").glob(".windows-*/output"))
                (output / release.release_info.windows_package_name(info())).write_bytes(b"NSIS fixture")
            with patch.object(release, "COMPONENT", component), patch.object(release, "prepare", return_value=(binary, info())), patch.object(release.notices, "stage", side_effect=stage), patch.object(release, "signature", side_effect=sign), patch.object(release, "verify_payload"), patch.object(release, "install_nsis", return_value="trusted-makensis.exe"), patch.object(release, "run", side_effect=compiler), patch.object(release, "verify") as verify:
                release.package("dev", unsigned=False)
            app_signed = calls.index(("sign", "maple-agent.exe"))
            compiled = calls.index(("compiler", "trusted-makensis.exe"))
            installer_signed = calls.index(("sign", release.release_info.windows_package_name(info())))
            self.assertLess(app_signed, compiled)
            self.assertLess(compiled, installer_signed)
            self.assertEqual([call for call in calls if call[0] == "compiler"], [("compiler", "trusted-makensis.exe")])
            self.assertFalse(verify.call_args.kwargs["smoke"])

    def test_nsis_trailing_install_override_is_not_requoted_or_split(self):
        command = '"C:\\setup with spaces.exe" /S /D=C:\\install with spaces'
        with patch.object(subprocess, "run", return_value=subprocess.CompletedProcess(command, 0)) as mocked:
            release.run(command)
        self.assertEqual(mocked.call_args.args[0], command)

    def test_nsis_archive_rejects_hash_mismatch_and_traversal(self):
        for name in ("../escape.exe", "nsis-3.12/../../escape", "nsis-3.12/C:/escape", "outside/makensis.exe"):
            with self.subTest(name=name), tempfile.TemporaryDirectory() as temporary:
                archive = Path(temporary) / "nsis.zip"
                with zipfile.ZipFile(archive, "w") as contents:
                    contents.writestr(name, b"fixture")
                with self.assertRaisesRegex(ValueError, "reviewed pin"):
                    release.unpack_nsis(archive, Path(temporary) / "output")
                with patch.object(release, "NSIS_SHA256", hashlib.sha256(archive.read_bytes()).hexdigest()):
                    with self.assertRaisesRegex(ValueError, "Unsafe NSIS"):
                        release.unpack_nsis(archive, Path(temporary) / "output")

    def test_generated_config_escapes_nsis_values_and_profiles_have_distinct_ids(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "configuration.nsh"
            release.write_nsis_config(path, info(), Path('C:/a $variable/path with "quote"'), Path("C:/setup.exe"), unsigned=True)
            text = path.read_text(encoding="utf-8")
            self.assertIn("$$variable", text)
            self.assertIn('$\\"quote$\\"', text)
            self.assertNotIn("!define SIGNER", text)
            self.assertNotEqual(info("dev")["bundle_id"], info("prod")["bundle_id"])
            self.assertNotEqual(info("dev")["display_name"], info("prod")["display_name"])


if __name__ == "__main__":
    unittest.main()
