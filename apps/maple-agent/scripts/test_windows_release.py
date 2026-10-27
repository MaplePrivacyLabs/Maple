#!/usr/bin/env python3
"""Windows package identity, extraction and signing-boundary regressions."""

import hashlib
import gzip
import importlib.util
import io
import json
from pathlib import Path
import re
import subprocess
import sys
import tarfile
import tempfile
import types
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
        "git_revision": "a" * 8, "update_feed_url": profiles[profile]["windows_update_feed_url"],
    }


class WindowsMetadataTests(unittest.TestCase):
    def package(self, directory, profile="dev", unsigned=True, build="12"):
        identity = info(profile)
        (directory / release.release_info.windows_package_name(identity)).write_bytes(b"installer fixture")
        (directory / release.release_info.windows_archive_name(identity)).write_bytes(b"bundle fixture")
        (directory / "build-info.json").write_text(json.dumps(identity))
        release.release_info.write_manifest(identity, directory, release.PLATFORM, unsigned, build)
        return identity

    def write_checksums(self, directory):
        (directory / "SHA256SUMS").write_text("".join(
            f"{hashlib.sha256(path.read_bytes()).hexdigest()}  {path.name}\n"
            for path in sorted(directory.iterdir()) if path.name != "SHA256SUMS"
        ))

    def test_windows_prod_has_its_own_feed_without_erasing_public_keys(self):
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
                    (directory / release.release_info.windows_archive_name(identity)).write_bytes(b"bundle fixture")
                    (directory / "build-info.json").write_text(json.dumps(identity))
                    release.release_info.write_manifest(identity, directory, release.PLATFORM, unsigned, "12")
                    manifest = release.release_info.verify_artifacts(profile, directory, unsigned, "a" * 40, release.PLATFORM)
                    self.assertEqual(manifest["windows_signing"], "unsigned" if unsigned else "authenticode")
                    self.assertEqual(manifest["updater"], release.release_info.windows_archive_name(identity))
                    self.assertEqual(manifest["build"], "12")
                    self.assertEqual({path.name for path in directory.iterdir()}, {
                        package.name, release.release_info.windows_archive_name(identity),
                        "build-info.json", "manifest.json", "SHA256SUMS",
                    })
                    self.assertEqual(set(manifest), {
                        "schema_version", "profile", "platform", "source_sha", "version",
                        "package", "distribution", "macos_signing", "windows_signing", "build", "updater",
                    })
                    self.assertTrue(all(path.stat().st_mode & 0o777 == 0o644 for path in directory.iterdir()))
                    with self.assertRaises(ValueError):
                        release.release_info.verify_artifacts(profile, directory, not unsigned, "a" * 40, release.PLATFORM)
                    with self.assertRaises(ValueError):
                        release.release_info.verify_artifacts(profile, directory, unsigned, "b" * 40, release.PLATFORM)
                    package.rename(directory / "renamed-setup.exe")
                    with self.assertRaises(ValueError):
                        release.release_info.write_manifest(identity, directory, release.PLATFORM, unsigned, "12")

    def test_unknown_installer_payload_cannot_be_blessed_by_new_checksums(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            identity = info()
            (directory / release.release_info.windows_package_name(identity)).write_bytes(b"installer fixture")
            (directory / "build-info.json").write_text(json.dumps(identity))
            (directory / "extra.exe").write_bytes(b"unexpected")
            with self.assertRaises(ValueError):
                release.release_info.write_manifest(identity, directory, release.PLATFORM, True, "12")

    def test_windows_manifest_requires_bundle_and_binds_build_and_inventory(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            identity = info()
            (directory / release.release_info.windows_package_name(identity)).write_bytes(b"installer")
            (directory / "build-info.json").write_text(json.dumps(identity))
            with self.assertRaisesRegex(ValueError, "updater bundle"):
                release.release_info.write_manifest(identity, directory, release.PLATFORM, True, "12")
            (directory / release.release_info.windows_archive_name(identity)).write_bytes(b"bundle")
            for build in (None, False, 12, "", "0", "01", "12x", "1" * 19):
                with self.subTest(build=build), self.assertRaisesRegex(ValueError, "build number"):
                    release.release_info.write_manifest(identity, directory, release.PLATFORM, True, build)

    def test_windows_manifest_cli_requires_an_explicit_build_number(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            self.package(directory)
            (directory / "manifest.json").unlink()
            (directory / "SHA256SUMS").unlink()
            command = [sys.executable, str(SCRIPTS / "release-info.py"), "manifest", "dev",
                       str(directory / "build-info.json"), "--output", str(directory),
                       "--platform", release.PLATFORM, "--unsigned"]
            result = subprocess.run(command, capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("Windows package build number", result.stderr)
            self.assertFalse((directory / "manifest.json").exists())
            self.assertFalse((directory / "SHA256SUMS").exists())
            subprocess.run([*command, "--build-number", "42"], check=True, capture_output=True, text=True)
            manifest = release.release_info.verify_artifacts("dev", directory, True, "a" * 40, release.PLATFORM)
            self.assertEqual(manifest["build"], "42")

    def test_downloaded_manifest_rejects_wrong_build_bundle_or_extra_fields_even_with_new_checksums(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            self.package(directory)
            path = directory / "manifest.json"
            original = json.loads(path.read_text())
            for field, value in (
                ("build", None), ("build", 12), ("build", False), ("build", "0"),
                ("build", "01"), ("build", "1" * 19),
                ("updater", "maple-agent-prod-0.1.2-aaaaaaaa-windows-x86_64.tar.gz"),
                ("updater", "https://wrong.invalid/bundle.tar.gz"),
                ("macos_signing", "developer-id-notarized"), ("windows_signing", "sha256"),
                ("unexpected", "public but not part of the manifest contract"),
            ):
                with self.subTest(field=field, value=value):
                    path.write_text(json.dumps({**original, field: value}))
                    self.write_checksums(directory)
                    with self.assertRaises(ValueError):
                        release.release_info.verify_artifacts("dev", directory, True, "a" * 40, release.PLATFORM)
            for field in ("build", "updater", "macos_signing", "windows_signing"):
                with self.subTest(missing=field):
                    path.write_text(json.dumps({key: value for key, value in original.items() if key != field}))
                    self.write_checksums(directory)
                    with self.assertRaises(ValueError):
                        release.release_info.verify_artifacts("dev", directory, True, "a" * 40, release.PLATFORM)

    def test_downloaded_inventory_cannot_be_renamed_or_extended_with_consistent_checksums(self):
        for mutation in ("renamed-installer", "renamed-bundle", "missing-installer", "missing-bundle", "extra"):
            with self.subTest(mutation=mutation), tempfile.TemporaryDirectory() as temporary:
                directory = Path(temporary)
                identity = self.package(directory)
                path = directory / "manifest.json"
                manifest = json.loads(path.read_text())
                if mutation == "extra":
                    (directory / "extra.txt").write_bytes(b"unexpected public file")
                else:
                    field = "package" if mutation.endswith("installer") else "updater"
                    package = directory / manifest[field]
                    if mutation.startswith("renamed"):
                        name = "renamed-setup.exe" if field == "package" else "renamed.tar.gz"
                        package.rename(directory / name)
                        manifest[field] = name
                        path.write_text(json.dumps(manifest))
                    else:
                        package.unlink()
                self.write_checksums(directory)
                with self.assertRaisesRegex(ValueError, "exactly the expected installer"):
                    release.release_info.verify_artifacts(identity["profile"], directory, True, "a" * 40, release.PLATFORM)

    def test_every_final_file_is_checksum_bound(self):
        for filename in ("build-info.json", "manifest.json", release.release_info.windows_package_name(info()),
                         release.release_info.windows_archive_name(info())):
            with self.subTest(filename=filename), tempfile.TemporaryDirectory() as temporary:
                directory = Path(temporary)
                self.package(directory)
                with (directory / filename).open("ab") as file:
                    file.write(b"\n")
                with self.assertRaisesRegex(ValueError, "checksum"):
                    release.release_info.verify_artifacts("dev", directory, True, "a" * 40, release.PLATFORM)

    def test_final_inventory_rejects_links_and_directories(self):
        for name in ("build-info.json", "manifest.json", "SHA256SUMS",
                     release.release_info.windows_package_name(info()), release.release_info.windows_archive_name(info())):
            for kind in ("symlink", "directory"):
                with self.subTest(name=name, kind=kind), tempfile.TemporaryDirectory() as temporary:
                    directory = Path(temporary)
                    self.package(directory)
                    path = directory / name
                    path.unlink()
                    if kind == "symlink":
                        path.symlink_to(directory / "absent-target")
                    else:
                        path.mkdir()
                    with self.assertRaisesRegex(ValueError, "unsupported file"):
                        release.release_info.verify_artifacts("dev", directory, True, "a" * 40, release.PLATFORM)


class BundleTests(unittest.TestCase):
    def payload(self, directory, identity=None, build="12"):
        identity = info() if identity is None else identity
        for name in release.PAYLOAD_FILES:
            file = directory / name
            file.parent.mkdir(parents=True, exist_ok=True)
            file.write_bytes(("payload " + name).encode())
        (directory / "bundle-info.json").write_text(json.dumps(release.bundle_descriptor(identity, build), sort_keys=True, separators=(",", ":")) + "\n")
        return directory

    def archive(self, path, members):
        with gzip.open(path, "wb") as compressed:
            with tarfile.open(fileobj=compressed, mode="w", format=tarfile.USTAR_FORMAT) as archive:
                for member, content in members:
                    archive.addfile(member, io.BytesIO(content) if content is not None else None)

    def test_bundle_is_deterministic_exact_generation_payload_and_matches_installer_bytes(self):
        with tempfile.TemporaryDirectory() as temporary:
            parent = Path(temporary)
            payload = self.payload(parent / "payload")
            first, second = parent / "first.tar.gz", parent / "second.tar.gz"
            release.write_bundle(payload, first, info(), "12")
            release.write_bundle(payload, second, info(), "12")
            self.assertEqual(first.read_bytes(), second.read_bytes())
            hashes = release.inspect_bundle(first, info(), "12", payload)
            self.assertEqual(set(hashes), {"bundle-info.json", "maple-agent.exe", "LICENSE.txt", "build-info.json"})
            self.assertEqual(release.BUNDLE_FORMAT, "maple-agent-windows-bundle-v2")
            self.assertNotIn("uninstall.exe", hashes)
            (payload / "maple-agent.exe").write_bytes(b"changed installer payload")
            with self.assertRaisesRegex(ValueError, "differs from"):
                release.inspect_bundle(first, info(), "12", payload)
            for changed_info, build in ((info("prod"), "12"), (info(), "13")):
                with self.assertRaisesRegex(ValueError, "identity"):
                    release.inspect_bundle(first, changed_info, build)

    def test_archive_rejects_links_case_aliases_duplicate_unknown_paths_and_extensions(self):
        with tempfile.TemporaryDirectory() as temporary:
            parent = Path(temporary)
            payload = self.payload(parent / "payload")
            valid = parent / "valid.tar.gz"
            release.write_bundle(payload, valid, info(), "12")
            with tarfile.open(valid, "r:gz") as archive:
                members = [(member, archive.extractfile(member).read() if member.isfile() else None) for member in archive]
            for name, kind in (("../escape", tarfile.REGTYPE), (release.BUNDLE_ROOT + "/MAPLE-AGENT.exe", tarfile.REGTYPE), (release.BUNDLE_ROOT + "/extra.dll", tarfile.REGTYPE), (members[-1][0].name, tarfile.REGTYPE), (release.BUNDLE_ROOT + "/link", tarfile.SYMTYPE), (release.BUNDLE_ROOT + "/long", tarfile.GNUTYPE_LONGNAME)):
                with self.subTest(name=name, kind=kind):
                    entry = tarfile.TarInfo(name)
                    entry.type, entry.mode = kind, 0o644
                    if kind == tarfile.SYMTYPE:
                        entry.linkname = "maple-agent.exe"
                    bad = parent / "bad.tar.gz"
                    self.archive(bad, [*members, (entry, b"")])
                    with self.assertRaises((ValueError, tarfile.TarError)):
                        release.inspect_bundle(bad, info(), "12")

    def test_archive_rejects_duplicate_descriptor_keys_truncated_and_trailing_content(self):
        with tempfile.TemporaryDirectory() as temporary:
            parent = Path(temporary)
            payload = self.payload(parent / "payload")
            valid = parent / "valid.tar.gz"
            release.write_bundle(payload, valid, info(), "12")
            with tarfile.open(valid, "r:gz") as archive:
                members = [(member, archive.extractfile(member).read() if member.isfile() else None) for member in archive]
            changed = []
            for entry, content in members:
                if entry.name.endswith("/bundle-info.json"):
                    content = content.replace(b'{', b'{"profile":"dev",', 1)
                    entry.size = len(content)
                changed.append((entry, content))
            bad = parent / "duplicate.tar.gz"
            self.archive(bad, changed)
            with self.assertRaisesRegex(ValueError, "duplicate"):
                release.inspect_bundle(bad, info(), "12")
            raw = gzip.decompress(valid.read_bytes())
            for content in (raw[:512], raw + b"not tar"):
                bad.write_bytes(gzip.compress(content))
                with self.assertRaises((ValueError, tarfile.TarError)):
                    release.inspect_bundle(bad, info(), "12")

    def test_bundle_rejects_unlisted_directories_and_links_before_writing(self):
        with tempfile.TemporaryDirectory() as temporary:
            parent = Path(temporary)
            payload = self.payload(parent / "payload")
            (payload / "extra").mkdir()
            with self.assertRaisesRegex(ValueError, "exact standalone"):
                release.write_bundle(payload, parent / "extra.tar.gz", info(), "12")
            (payload / "extra").rmdir()
            (payload / "extra").symlink_to(parent, target_is_directory=True)
            with self.assertRaisesRegex(ValueError, "links or reparse"):
                release.write_bundle(payload, parent / "link.tar.gz", info(), "12")
            self.assertFalse((parent / "extra.tar.gz").exists())
            self.assertFalse((parent / "link.tar.gz").exists())


class RuntimeClosureTests(unittest.TestCase):
    def test_os_components_and_api_sets_are_allowed_but_compiler_runtime_is_not(self):
        # ICU is an OS component from Win10 1703; WER is an OS component from
        # Vista. Both appeared in the actual optimized Agent executable.
        allowed = ["kernel32.dll", "dwrite.dll", "d3d11.dll", "uiautomationcore.dll",
                   "icuuc.dll", "wer.dll",
                   "api-ms-win-core-synch-l1-2-0.dll"]
        with patch.object(release.pe, "inspect_pe", return_value={"imports": allowed}):
            release.validate_imports(Path("unused.exe"))
        for unexpected in ("vcruntime140.dll", "msvcp140.dll", "libcrypto.dll", "onnxruntime.dll", "icuuc999.dll", "api-ms-win-../../evil.dll", "unknown.dll"):
            with self.subTest(unexpected=unexpected), patch.object(release.pe, "inspect_pe", return_value={"imports": allowed + [unexpected]}):
                with self.assertRaisesRegex(ValueError, "non-OS DLL"):
                    release.validate_imports(Path("unused.exe"))


class InstallerExtractionTests(unittest.TestCase):
    def expected_names(self):
        return sorted({"versions/0.1.2-12/" + name for name in release.BUNDLE_FILES} | {"maple-agent.exe", "uninstall.exe", release.INSTALLER_CHECKER})

    def member_data(self, name):
        if name.endswith("/bundle-info.json"):
            return (json.dumps(release.bundle_descriptor(info(), "12"), sort_keys=True, separators=(",", ":")) + "\n").encode()
        return b"fixture content!!"

    def inventory(self, names):
        return "\n\n".join(f"Path = {name}\nSize = {len(self.member_data(name))}\nFolder = -" for name in names) + "\n"

    def extract(self, directory, names, unsigned):
        def command(arguments, **_options):
            if arguments[1] == "l":
                return subprocess.CompletedProcess(arguments, 0, self.inventory(names))
            return subprocess.CompletedProcess(arguments, 0)
        def member(_extractor, _installer, name, destination, _limit):
            destination.parent.mkdir(parents=True, exist_ok=True)
            data = self.member_data(name)
            destination.write_bytes(data)
            return len(data)
        with patch.object(release, "seven_zip", return_value="7z"), patch.object(release, "run", side_effect=command), patch.object(release, "extract_member", side_effect=member):
            return release.extract_installer(Path("fixture-setup.exe"), directory, unsigned, info(), "12")

    def test_exact_signed_and_unsigned_payload_inventories(self):
        for unsigned, uninstaller in ((False, "uninstall.exe"), (True, "uninstall.exe")):
            with self.subTest(unsigned=unsigned), tempfile.TemporaryDirectory() as temporary:
                directory = Path(temporary)
                result = self.extract(directory, self.expected_names(), unsigned)
                self.assertEqual(result, directory / uninstaller)

    def test_rejects_traversal_duplicate_extra_and_missing_before_extraction(self):
        valid = self.expected_names()
        for names in (valid + ["../escape"], valid + ["C:/escape"], valid + ["extra.dll"], valid + [valid[0]], valid[:-1]):
            with self.subTest(names=names), tempfile.TemporaryDirectory() as temporary:
                directory = Path(temporary)
                with self.assertRaises(ValueError):
                    self.extract(directory, names, True)
                self.assertEqual(list(directory.iterdir()), [])

    def test_reconstructed_uninstaller_can_omit_preliminary_size(self):
        names = self.expected_names()
        listing = self.inventory(names).replace("Path = uninstall.exe\nSize = 17", "Path = uninstall.exe\nSize = ")
        with tempfile.TemporaryDirectory() as temporary, patch.object(self, "inventory", return_value=listing):
            self.assertEqual(self.extract(Path(temporary), names, True).name, "uninstall.exe")

    def test_temporary_preflight_checker_must_equal_exact_generation_executable(self):
        original = self.member_data
        def changed(name):
            return b"changed checker!!" if name == release.INSTALLER_CHECKER else original(name)
        with tempfile.TemporaryDirectory() as temporary, patch.object(self, "member_data", side_effect=changed):
            with self.assertRaisesRegex(ValueError, "checker differs"):
                self.extract(Path(temporary), self.expected_names(), True)

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
    def test_extraction_cleanup_failure_preserves_the_original_smoke_error(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            extraction = directory / "extracted"
            extraction.mkdir()
            manifest = {"package": "installer.exe", "updater": "bundle.tar.gz", "build": "12"}
            primary = ValueError("original installer failure")
            def cleanup():
                raise PermissionError("secondary extraction cleanup failure")
            proxy = types.SimpleNamespace(name=str(extraction), cleanup=cleanup)
            diagnostics = io.StringIO()
            with patch.object(release.tempfile, "TemporaryDirectory", return_value=proxy), patch.dict(release.os.environ, MAPLE_PACKAGE_BUILD_NUMBER="12", clear=True), patch.object(release, "source_sha", return_value="a" * 40), patch.object(release.release_info, "verify_artifacts", return_value=manifest), patch.object(release.release_info, "read_info", return_value=info()), patch.object(release, "signature"), patch.object(release, "extract_installer", return_value=extraction / "uninstall.exe"), patch.object(release, "verify_payload"), patch.object(release, "inspect_bundle", return_value={}), patch.object(release, "smoke_installer", side_effect=primary), patch.object(sys, "stderr", diagnostics):
                with self.assertRaises(ValueError) as raised:
                    release.verify("dev", directory, True, smoke=True)
            self.assertIs(raised.exception, primary)
            self.assertIn("Windows verification cleanup failure: PermissionError", diagnostics.getvalue())

    def test_smoke_keeps_the_verified_extracted_checker_alive(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            manifest = {"package": "installer.exe", "updater": "bundle.tar.gz", "build": "12"}
            checked = []
            def extract(_installer, payload, *_arguments):
                checker = payload / release.INSTALLER_CHECKER
                checker.parent.mkdir()
                checker.write_bytes(b"verified checker fixture")
                return payload / "uninstall.exe"
            def smoke(*_arguments, checker):
                self.assertEqual(checker.read_bytes(), b"verified checker fixture")
                checked.append(checker)
            with patch.dict(release.os.environ, MAPLE_PACKAGE_BUILD_NUMBER="12", clear=True), patch.object(release, "source_sha", return_value="a" * 40), patch.object(release.release_info, "verify_artifacts", return_value=manifest), patch.object(release.release_info, "read_info", return_value=info()), patch.object(release, "signature"), patch.object(release, "extract_installer", side_effect=extract), patch.object(release, "verify_payload"), patch.object(release, "inspect_bundle", return_value={}), patch.object(release, "smoke_installer", side_effect=smoke):
                release.verify("dev", directory, True, smoke=True)
            self.assertEqual(len(checked), 1)
            self.assertFalse(checked[0].exists())

    def test_downloaded_verifier_checks_the_standalone_uninstaller_in_both_policies(self):
        for unsigned in (False, True):
            with self.subTest(unsigned=unsigned), tempfile.TemporaryDirectory() as temporary:
                directory = Path(temporary)
                manifest = {"package": "installer.exe", "updater": "bundle.tar.gz", "build": "12"}
                with patch.dict(release.os.environ, MAPLE_PACKAGE_BUILD_NUMBER="12"), patch.object(release, "source_sha", return_value="a" * 40), patch.object(release.release_info, "verify_artifacts", return_value=manifest), patch.object(release.release_info, "read_info", return_value=info()), patch.object(release, "signature") as signature, patch.object(release, "extract_installer", side_effect=lambda _installer, payload, *_args: payload / "uninstall.exe"), patch.object(release, "verify_payload"), patch.object(release, "inspect_bundle", return_value={}), patch.object(release, "smoke_installer") as smoke:
                    release.verify("dev", directory, unsigned, smoke=False)
                self.assertEqual([call.args[0].name for call in signature.call_args_list], ["installer.exe", "uninstall.exe"])
                self.assertTrue(all(call.args[1] == unsigned for call in signature.call_args_list))
                smoke.assert_not_called()

    def test_packager_signs_payload_before_compression_and_final_output_afterward_without_running_app(self):
        with tempfile.TemporaryDirectory() as temporary:
            component = Path(temporary)
            binary = component / "target/release/maple-agent.exe"
            binary.parent.mkdir(parents=True)
            binary.write_bytes(b"downloaded executable must not run")
            (component / "LICENSE").write_text("license fixture")
            calls = []
            def sign(file, unsigned, *, sign=False):
                calls.append(("sign" if sign else "verify-signature", Path(file).name))
            def compiler(arguments, **_options):
                calls.append(("compiler", str(arguments[0])))
                output = next((component / "dist").glob(".windows-*/output"))
                (output / release.release_info.windows_package_name(info())).write_bytes(b"NSIS fixture")
            def extract(_installer, directory, _unsigned, identity, build):
                (directory / "versions" / (identity["version"] + "-" + build)).mkdir(parents=True)
                uninstaller = directory / "uninstall.exe"
                uninstaller.write_bytes(b"uninstaller fixture")
                return uninstaller
            def bundle(_payload, archive, _info, _build):
                calls.append(("bundle", Path(archive).name))
                archive.write_bytes(b"bundle fixture")
            with patch.dict(release.os.environ, MAPLE_PACKAGE_BUILD_NUMBER="12"), patch.object(release, "COMPONENT", component), patch.object(release, "prepare", return_value=(binary, info())), patch.object(release, "signature", side_effect=sign), patch.object(release, "verify_payload"), patch.object(release, "install_nsis", return_value="trusted-makensis.exe"), patch.object(release, "run", side_effect=compiler), patch.object(release, "extract_installer", side_effect=extract), patch.object(release, "write_bundle", side_effect=bundle), patch.object(release, "verify") as verify:
                release.package("dev", unsigned=False)
            app_signed = calls.index(("sign", "maple-agent.exe"))
            compiled = calls.index(("compiler", "trusted-makensis.exe"))
            installer_signed = calls.index(("sign", release.release_info.windows_package_name(info())))
            self.assertLess(app_signed, compiled)
            self.assertLess(compiled, installer_signed)
            self.assertLess(installer_signed, calls.index(("bundle", release.release_info.windows_archive_name(info()))))
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
            release.write_nsis_config(path, info(), Path('C:/a $variable/path with "quote"'), Path("C:/setup.exe"), unsigned=True, build="12")
            text = path.read_text(encoding="utf-8")
            self.assertIn("$$variable", text)
            self.assertIn('$\\"quote$\\"', text)
            self.assertIn("!define SIGNER", text)
            self.assertIn('!define UNSIGNED_UNINSTALLER "1"', text)
            self.assertIn('!define GENERATION "0.1.2-12"', text)
            self.assertNotEqual(info("dev")["bundle_id"], info("prod")["bundle_id"])
            self.assertNotEqual(info("dev")["display_name"], info("prod")["display_name"])

    def test_nsis_native_preflight_precedes_installed_files_and_pointer_activation(self):
        source = (SCRIPTS / "windows-installer.nsi").read_text()
        preflight = source.index('--internal-windows-installer-preflight "$INSTDIR"')
        prepare = source.index('--internal-windows-installer-prepare "$INSTDIR" ${GENERATION}')
        self.assertLess(preflight, prepare)
        self.assertLess(prepare, source.index('SetOutPath "$INSTDIR"'))
        self.assertLess(preflight, source.index('WriteRegStr HKCU'))
        self.assertLess(source.index('File "${PAYLOAD}\\bundle-info.json"'), source.index('--internal-windows-installer-activate ${GENERATION}'))
        self.assertNotIn('File /oname=current.json', source)
        self.assertNotIn('RMDir /r', source)
        self.assertIn('--internal-windows-uninstall-cleanup', source)
        cleanup = source.index('--internal-windows-uninstall-cleanup')
        self.assertLess(cleanup, source.index('Delete "$INSTDIR\\.install.lock"'))
        self.assertLess(cleanup, source.index('Delete "$INSTDIR\\.installer-intent.json"'))
        self.assertIn('IfFileExists "$INSTDIR\\maple-agent.exe" bootstrap_ready', source)
        self.assertIn('IfFileExists "$INSTDIR\\uninstall.exe" uninstaller_ready', source)

    def test_nsis_helper_launch_errors_cannot_reuse_a_previous_success_exit_code(self):
        source = (SCRIPTS / "windows-installer.nsi").read_text()
        instructions = [line.strip() for line in source.splitlines() if line.strip() and not line.lstrip().startswith(';')]
        calls = [index for index, line in enumerate(instructions) if line.startswith('ExecWait ')]
        self.assertEqual(len(calls), 4)
        for index in calls:
            command = instructions[index]
            destination = 'failed' if '--internal-windows-installer-activate ' in command else (
                'refuse' if '--internal-windows-uninstall-cleanup' in command else 'refused'
            )
            with self.subTest(command=command):
                # NSIS leaves the output variable undefined on launch error.
                # Test its error flag before ever reading a stale zero in $0.
                self.assertEqual(instructions[index - 1], 'ClearErrors')
                self.assertEqual(instructions[index + 1], 'IfErrors ' + destination)
                child = {'refused': (22, 24), 'failed': (41,), 'refuse': (52,)}[destination]
                self.assertIn(instructions[index + 2], ['StrCpy $FailureCode ' + str(code) for code in child])
                self.assertEqual(instructions[index + 3], 'StrCmp $0 0 0 ' + destination)

    def test_nsis_exit_codes_identify_probe_phase_and_launch_vs_child_failures(self):
        source = (SCRIPTS / "windows-installer.nsi").read_text()
        codes = {int(value) for value in re.findall(r'StrCpy \$FailureCode ([0-9]+)', source)}
        self.assertEqual(codes, set(release.NSIS_FAILURE_PHASES))
        for flag, launch, child in (("installer-preflight", 21, 22), ("installer-prepare", 23, 24),
                                    ("installer-activate", 40, 41), ("uninstall-cleanup", 51, 52)):
            with self.subTest(flag=flag):
                pattern = (r'StrCpy \$FailureCode ' + str(launch) + r'\s+ClearErrors\s+ExecWait [^\n]*--internal-windows-' + flag
                           + r'[^\n]*\n\s+IfErrors \w+\s+StrCpy \$FailureCode ' + str(child))
                self.assertRegex(source, pattern)
        self.assertRegex(source, r'ClearErrors\s+GetDLLVersion [^\n]+\n\s+IfErrors unsupported')
        self.assertIn('SetErrorLevel 0', source)
        self.assertIn('SetErrorLevel $FailureCode', source)

    def test_nsis_uninstall_keeps_native_fence_until_all_mutations_are_finished(self):
        source = (SCRIPTS / "windows-installer.nsi").read_text().split('Section "Uninstall"', 1)[1]
        release_fence = 'Delete "$INSTDIR\\.uninstaller-intent.json"'
        self.assertEqual(source.count(release_fence), 1)
        before, after = source.split(release_fence)
        for mutation in ('Delete "$INSTDIR\\current.json"', 'Delete "$INSTDIR\\.install.lock"',
                         'Delete "$INSTDIR\\maple-agent.exe"', 'Delete "$INSTDIR\\uninstall.exe"',
                         'Delete "$SMPROGRAMS\\${DISPLAY_NAME}\\${DISPLAY_NAME}.lnk"',
                         'DeleteRegKey HKCU'):
            with self.subTest(mutation=mutation):
                self.assertIn(mutation, before)
        mutation = re.compile(r'^(?:Delete(?:RegKey|RegValue)?|RMDir|Write\w*|CreateDirectory|CreateShortcut|SetOutPath|File)\b')
        remaining = [line.strip() for line in after.splitlines() if mutation.match(line.strip())]
        self.assertEqual(remaining, [])


class SmokeDiagnosticsTests(unittest.TestCase):
    def registry(self):
        def missing(*_arguments):
            raise FileNotFoundError
        return types.SimpleNamespace(KEY_READ=1, KEY_WOW64_32KEY=2, HKEY_CURRENT_USER=3, OpenKey=missing)

    def test_direct_preflight_captures_both_streams_and_bounds_output(self):
        result = release.capture_preflight([sys.executable, "-c", "import sys; print('stdout'); print('stderr', file=sys.stderr)"], dict(release.os.environ))
        self.assertEqual(result.returncode, 0)
        self.assertIn(b"stdout", result.stdout)
        self.assertIn(b"stderr", result.stdout)
        diagnostics = io.StringIO()
        with patch.object(sys, "stderr", diagnostics), self.assertRaisesRegex(ValueError, "diagnostic bounds"):
            release.capture_preflight([sys.executable, "-c", "import sys; sys.stderr.buffer.write(b'x' * 65536)"], dict(release.os.environ))
        self.assertLessEqual(len(diagnostics.getvalue()), release.DIAGNOSTIC_LIMIT + 100)

    def test_direct_preflight_timeout_is_bounded(self):
        with patch.object(sys, "stderr", io.StringIO()), self.assertRaises(subprocess.TimeoutExpired):
            release.capture_preflight([sys.executable, "-c", "import time; time.sleep(2)"], dict(release.os.environ), timeout=0.02)

    def test_first_installer_failure_preserves_phase_and_evidence_before_failed_temp_cleanup(self):
        with tempfile.TemporaryDirectory() as temporary:
            scratch = Path(temporary) / "scratch"
            scratch.mkdir()
            diagnostics = io.StringIO()
            primary = subprocess.CalledProcessError(24, "fixture installer")
            environment = {"FIXTURE": "same sanitized environment"}
            def install(command, **options):
                self.assertIs(options["env"], environment)
                root = Path(command.split("/D=", 1)[1])
                root.mkdir()
                (root / "current.json").write_text("do not print pointer contents")
                (root / ".installer-intent.json").write_text("do not print intent contents")
                raise primary
            def cleanup():
                self.assertIn('"installer_phase": "prepare-child"', diagnostics.getvalue())
                self.assertIn('"current.json": "file"', diagnostics.getvalue())
                raise PermissionError("do not print cleanup details")
            proxy = types.SimpleNamespace(name=str(scratch), cleanup=cleanup)
            with patch.object(release.tempfile, "TemporaryDirectory", return_value=proxy), patch.dict(sys.modules, winreg=self.registry()), patch.object(release, "smoke_environment", return_value=environment), patch.object(release, "capture_preflight", return_value=subprocess.CompletedProcess("checker", 0, b"")) as probe, patch.object(release, "run", side_effect=install) as commands, patch.object(sys, "stderr", diagnostics):
                with self.assertRaises(subprocess.CalledProcessError) as raised:
                    release.smoke_installer(Path("fixture.exe"), info(), False, "12", {}, checker=Path("verified-checker.exe"))
            self.assertIs(raised.exception, primary)
            self.assertEqual(commands.call_count, 1)
            self.assertIs(probe.call_args.args[1], environment)
            self.assertEqual(probe.call_args.args[0][1], "--internal-windows-installer-preflight")
            self.assertIn("Windows verification cleanup failure: PermissionError", diagnostics.getvalue())
            self.assertNotIn("do not print", diagnostics.getvalue())

    def test_cleanup_command_failure_never_masks_the_primary_failure(self):
        registry = self.registry()
        primary = ValueError("primary verification failure")
        queries = 0
        def query(*_arguments):
            nonlocal queries
            queries += 1
            if queries == 2:
                raise primary
            raise FileNotFoundError
        registry.OpenKey = query
        calls = 0
        def command(arguments, **_options):
            nonlocal calls
            calls += 1
            if calls == 1:
                root = Path(arguments.split("/D=", 1)[1])
                root.mkdir()
                (root / "maple-agent.exe").write_bytes(b"owned fixture")
                return subprocess.CompletedProcess(arguments, 0)
            raise subprocess.CalledProcessError(1, "cleanup fixture")
        diagnostics = io.StringIO()
        with patch.dict(sys.modules, winreg=registry), patch.object(release, "smoke_environment", return_value={}), patch.object(release, "capture_preflight", return_value=subprocess.CompletedProcess("checker", 0, b"")), patch.object(release, "run", side_effect=command), patch.object(sys, "stderr", diagnostics):
            with self.assertRaises(ValueError) as raised:
                release.smoke_installer(Path("fixture.exe"), info(), False, "12", {}, checker=Path("verified-checker.exe"))
        self.assertIs(raised.exception, primary)
        self.assertEqual(calls, 2)
        self.assertIn('"phase": "installed-verification"', diagnostics.getvalue())
        self.assertIn('"phase": "host-stop"', diagnostics.getvalue())

    def test_failed_readonly_preflight_never_runs_the_installer_or_prepare(self):
        diagnostics = io.StringIO()
        result = subprocess.CompletedProcess(["checker", "--internal-windows-installer-preflight"], 1, b"fixed native guard failure\x1b[31m\x00")
        with patch.dict(sys.modules, winreg=self.registry()), patch.object(release, "smoke_environment", return_value={}), patch.object(release, "capture_preflight", return_value=result), patch.object(release, "run") as commands, patch.object(sys, "stderr", diagnostics):
            with self.assertRaises(subprocess.CalledProcessError):
                release.smoke_installer(Path("fixture.exe"), info(), False, "12", {}, checker=Path("verified-checker.exe"))
        commands.assert_not_called()
        self.assertIn("fixed native guard failure", diagnostics.getvalue())
        self.assertNotIn("\x1b", diagnostics.getvalue())
        self.assertNotIn("\x00", diagnostics.getvalue())


class UnsignedProdSmokeTests(unittest.TestCase):
    def registry(self):
        def missing(*_arguments):
            raise FileNotFoundError
        return types.SimpleNamespace(KEY_READ=1, KEY_WOW64_32KEY=2, HKEY_CURRENT_USER=3, OpenKey=missing)

    def smoke(self, invoke):
        # This portable test covers the verifier's negative-proof assertions.
        # Actual NSIS/native trust behavior remains a Windows consumer test.
        with patch.dict(sys.modules, winreg=self.registry()), patch.object(release, "smoke_environment", return_value={}), patch.object(release, "capture_preflight", return_value=subprocess.CompletedProcess("checker", 1, b"expected unsigned refusal")), patch.object(subprocess, "run", side_effect=invoke), patch.object(release, "gui_smoke") as gui:
            release.smoke_installer(Path("preview.exe"), info("prod"), True, "12", {}, checker=Path("checker.exe"))
            gui.assert_not_called()

    def test_unsigned_prod_requires_failure_without_destination_changes(self):
        self.smoke(lambda *_arguments, **_options: subprocess.CompletedProcess("preview", 1))
        with self.assertRaisesRegex(ValueError, "refuse before"):
            self.smoke(lambda *_arguments, **_options: subprocess.CompletedProcess("preview", 0))

    def test_unsigned_prod_rejects_partial_destination_changes_even_after_failure(self):
        def partial(command, **_options):
            Path(command.split("/D=", 1)[1]).mkdir()
            return subprocess.CompletedProcess(command, 1)
        with self.assertRaisesRegex(ValueError, "refuse before"):
            self.smoke(partial)

    def test_unsigned_prod_rejects_registration_changes_even_after_failure(self):
        registry = self.registry()
        class Opened:
            def __enter__(self):
                return self
            def __exit__(self, *_arguments):
                return False
        calls = 0
        def changed(*_arguments):
            nonlocal calls
            calls += 1
            if calls == 1:
                raise FileNotFoundError
            return Opened()
        registry.OpenKey = changed
        with patch.dict(sys.modules, winreg=registry), patch.object(release, "smoke_environment", return_value={}), patch.object(release, "capture_preflight", return_value=subprocess.CompletedProcess("checker", 1, b"expected unsigned refusal")), patch.object(subprocess, "run", return_value=subprocess.CompletedProcess("preview", 1)):
            with self.assertRaisesRegex(ValueError, "registration"):
                release.smoke_installer(Path("preview.exe"), info("prod"), True, "12", {}, checker=Path("checker.exe"))


if __name__ == "__main__":
    unittest.main()
