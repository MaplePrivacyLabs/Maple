#!/usr/bin/env python3
"""Windows package identity, extraction and signing-boundary regressions."""

import ast
import ctypes
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
from unittest.mock import Mock, patch
import zipfile


SCRIPTS = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("windows_release", SCRIPTS / "windows-release.py")
release = importlib.util.module_from_spec(spec)
spec.loader.exec_module(release)


def info(profile="dev", arch="x86_64"):
    profiles = json.loads((SCRIPTS.parent / "release-profiles.json").read_text())
    return {
        **{key: profiles[profile][key] for key in release.release_info.PUBLIC_FIELDS},
        "profile": profile, "version": "0.1.2", "source_sha": "a" * 40,
        "git_revision": "a" * 8, "update_feed_url": profiles[profile]["windows_arm64_update_feed_url" if arch == "aarch64" else "windows_update_feed_url"],
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

    def test_arm64_packages_bind_their_own_names_inventory_and_feed(self):
        platform = "windows-aarch64"
        for profile in ("dev", "prod"):
            for unsigned in (True, False):
                with self.subTest(profile=profile, unsigned=unsigned), tempfile.TemporaryDirectory() as temporary:
                    directory = Path(temporary)
                    identity = info(profile, "aarch64")
                    installer = release.release_info.windows_package_name(identity, platform)
                    archive = release.release_info.windows_archive_name(identity, platform)
                    self.assertIn("-windows-aarch64-setup.exe", installer)
                    self.assertNotEqual(installer, release.release_info.windows_package_name(identity))
                    (directory / installer).write_bytes(b"ARM64 installer fixture")
                    (directory / archive).write_bytes(b"ARM64 generation fixture")
                    (directory / "build-info.json").write_text(json.dumps(identity))
                    release.release_info.write_manifest(identity, directory, platform, unsigned, "12")
                    manifest = release.release_info.verify_artifacts(profile, directory, unsigned, "a" * 40, platform)
                    self.assertEqual(manifest["platform"], platform)
                    self.assertEqual(manifest["package"], installer)
                    self.assertEqual(manifest["updater"], archive)
                    self.assertEqual(len(list(directory.iterdir())), 5)
                    with self.assertRaisesRegex(ValueError, "platform"):
                        release.release_info.verify_artifacts(profile, directory, unsigned, "a" * 40, "windows-x86_64")
                    (directory / installer).rename(directory / release.release_info.windows_package_name(identity))
                    with self.assertRaisesRegex(ValueError, "Windows artifact"):
                        release.release_info.write_manifest(identity, directory, platform, unsigned, "12")

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
    def test_payload_verifier_requires_the_exact_profile_icon_in_the_final_pe(self):
        import test_windows_build_info as fixture

        for profile in ("dev", "prod"):
            with self.subTest(profile=profile), tempfile.TemporaryDirectory() as temporary:
                directory = Path(temporary)
                identity = info(profile)
                (directory / "build-info.json").write_text(json.dumps(identity))
                (directory / "LICENSE.txt").write_bytes((release.COMPONENT / "LICENSE").read_bytes())
                binary = directory / "maple-agent.exe"
                canonical = release.COMPONENT / "app/packaging" / ("maple-agent-dev.ico" if profile == "dev" else "maple-agent.ico")
                other = canonical.with_name("maple-agent.ico" if profile == "dev" else "maple-agent-dev.ico")
                with patch.object(release, "source_sha", return_value="a" * 40):
                    binary.write_bytes(fixture.pe(identity, resources=fixture.icon_records(canonical.read_bytes())))
                    release.verify_payload(profile, directory, identity, True, verify_signature=False)
                    binary.write_bytes(fixture.pe(identity, resources=fixture.icon_records(other.read_bytes())))
                    with self.assertRaisesRegex(ValueError, "canonical profile icon"):
                        release.verify_payload(profile, directory, identity, True, verify_signature=False)

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
            (extraction / "uninstall.exe").write_bytes(b"verified uninstaller fixture")
            manifest = {"package": "installer.exe", "updater": "bundle.tar.gz", "build": "12"}
            primary = ValueError("original installer failure")
            def cleanup():
                raise PermissionError("secondary extraction cleanup failure")
            proxy = types.SimpleNamespace(name=str(extraction), cleanup=cleanup)
            diagnostics = io.StringIO()
            with patch.object(release.tempfile, "TemporaryDirectory", return_value=proxy), patch.dict(release.os.environ, MAPLE_PACKAGE_BUILD_NUMBER="12", clear=True), patch.object(release, "source_sha", return_value="a" * 40), patch.object(release.release_info, "verify_artifacts", return_value=manifest), patch.object(release.release_info, "read_info", return_value=info()), patch.object(release, "signature"), patch.object(release, "extract_installer", return_value=extraction / "uninstall.exe"), patch.object(release, "verify_payload"), patch.object(release, "inspect_bundle", return_value={}), patch.object(release, "require_native_host"), patch.object(release, "smoke_installer", side_effect=primary), patch.object(sys, "stderr", diagnostics):
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
                (payload / "uninstall.exe").write_bytes(b"verified uninstaller fixture")
                return payload / "uninstall.exe"
            def smoke(*_arguments, checker, uninstaller_sha256, arch):
                self.assertEqual(uninstaller_sha256, hashlib.sha256(b"verified uninstaller fixture").hexdigest())
                self.assertEqual(arch, "x86_64")
                self.assertEqual(checker.read_bytes(), b"verified checker fixture")
                checked.append(checker)
            with patch.dict(release.os.environ, MAPLE_PACKAGE_BUILD_NUMBER="12", clear=True), patch.object(release, "source_sha", return_value="a" * 40), patch.object(release.release_info, "verify_artifacts", return_value=manifest), patch.object(release.release_info, "read_info", return_value=info()), patch.object(release, "signature"), patch.object(release, "extract_installer", side_effect=extract), patch.object(release, "verify_payload"), patch.object(release, "inspect_bundle", return_value={}), patch.object(release, "require_native_host"), patch.object(release, "smoke_installer", side_effect=smoke):
                release.verify("dev", directory, True, smoke=True)
            self.assertEqual(len(checked), 1)
            self.assertFalse(checked[0].exists())

    def test_downloaded_arm_verifier_forwards_selected_target_into_installed_smoke(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            manifest = {"package": "installer.exe", "updater": "bundle.tar.gz", "build": "12"}
            def extract(_installer, payload, *_arguments):
                uninstaller = payload / "uninstall.exe"
                uninstaller.write_bytes(b"verified uninstaller fixture")
                return uninstaller
            with patch.dict(release.os.environ, MAPLE_PACKAGE_BUILD_NUMBER="12", clear=True), patch.object(release, "source_sha", return_value="a" * 40), patch.object(release.release_info, "verify_artifacts", return_value=manifest), patch.object(release.release_info, "read_info", return_value=info("dev", "aarch64")), patch.object(release, "signature"), patch.object(release, "extract_installer", side_effect=extract), patch.object(release, "verify_payload") as payload, patch.object(release, "inspect_bundle", return_value={}), patch.object(release, "require_native_host") as host, patch.object(release, "smoke_installer") as installed:
                release.verify("dev", directory, True, smoke=True, arch="aarch64")
            self.assertEqual(payload.call_args.kwargs["arch"], "aarch64")
            host.assert_called_once_with("aarch64")
            self.assertEqual(installed.call_args.kwargs["arch"], "aarch64")

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

    def test_generated_installer_contract_binds_native_architecture_and_supported_os(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "configuration.nsh"
            for arch, machine, minimum in (("x86_64", "AMD64", "15063"), ("aarch64", "ARM64", "22000")):
                with self.subTest(arch=arch):
                    release.write_nsis_config(path, info("prod", arch), Path("payload"), Path("setup.exe"), False, "12", arch)
                    definitions = dict(re.findall(r'^!define ([A-Z_]+) "(.*)"$', path.read_text(encoding="utf-8"), re.MULTILINE))
                    self.assertEqual(definitions["NATIVE_ARCHITECTURE"], machine)
                    self.assertEqual(definitions["MIN_WINDOWS_BUILD"], minimum)
            with self.assertRaisesRegex(ValueError, "architecture"):
                release.write_nsis_config(path, info(), Path("payload"), Path("setup.exe"), False, "12", "arm64ec")

    def test_smoke_requires_selected_native_host_instead_of_compatible_emulation(self):
        for native, matching, other in ((0x8664, "x86_64", "aarch64"), (0xAA64, "aarch64", "x86_64")):
            with self.subTest(native=native):
                kernel = Mock()
                def machine(_process, process, host):
                    # The diagnostic Python may itself be emulated; only the
                    # native host plus the separately checked Maple PE matters.
                    ctypes.memmove(process, (0x8664).to_bytes(2, "little"), 2)
                    ctypes.memmove(host, native.to_bytes(2, "little"), 2)
                    return 1
                kernel.IsWow64Process2.side_effect = machine
                with patch.object(ctypes, "WinDLL", return_value=kernel, create=True):
                    release.require_native_host(matching)
                    with self.assertRaisesRegex(ValueError, "native machine architecture"):
                        release.require_native_host(other)

    def test_generated_config_selects_the_profile_artwork_from_the_source_tree(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "configuration.nsh"
            for profile, artwork in (("dev", "maple-agent-dev.ico"), ("prod", "maple-agent.ico")):
                with self.subTest(profile=profile):
                    release.write_nsis_config(path, info(profile), Path("payload"), Path("setup.exe"), False, "12")
                    expected = release.nsis_literal(release.COMPONENT / "app/packaging" / artwork)
                    self.assertIn(f'!define ICON "{expected}"\n', path.read_text(encoding="utf-8"))

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
                # Test its error flag before ever reading a stale child status.
                self.assertEqual(instructions[index - 1], 'ClearErrors')
                self.assertEqual(instructions[index + 1], 'IfErrors ' + destination)
                child = {'refused': (22, 24), 'failed': (41,), 'refuse': (52,)}[destination]
                following = instructions[index + 2:]
                compare = next(index for index, line in enumerate(following) if line.startswith('StrCmp '))
                metadata = following[:compare]
                self.assertTrue(all(line.startswith('StrCpy ') for line in metadata))
                output = command.rsplit(' ', 1)[1]
                self.assertNotIn(output, [line.split(maxsplit=2)[1] for line in metadata])
                self.assertEqual([line for line in metadata if line.startswith('StrCpy $FailureCode ')],
                                 [next(line for line in metadata if line in ['StrCpy $FailureCode ' + str(code) for code in child])])
                self.assertEqual(following[compare], 'StrCmp ' + output + ' 0 0 ' + destination)

    def test_nsis_exit_codes_identify_probe_phase_and_launch_vs_child_failures(self):
        source = "\n".join(line for line in (SCRIPTS / "windows-installer.nsi").read_text().splitlines()
                           if not line.lstrip().startswith(';'))
        codes = {int(value) for value in re.findall(r'StrCpy \$FailureCode ([0-9]+)', source)}
        self.assertEqual(codes, set(release.NSIS_FAILURE_PHASES))
        for flag, launch, child in (("installer-preflight", 21, 22), ("installer-prepare", 23, 24),
                                    ("installer-activate", 40, 41), ("uninstall-cleanup", 51, 52)):
            with self.subTest(flag=flag):
                metadata = r'(?:\s+StrCpy (?!\$FailureCode)[^\n]+)*'
                pattern = (r'StrCpy \$FailureCode ' + str(launch) + metadata + r'\s+ClearErrors\s+ExecWait [^\n]*--internal-windows-' + flag
                           + r'[^\n]*\n\s+IfErrors \w+' + metadata + r'\s+StrCpy \$FailureCode ' + str(child))
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


class InteractiveInstallerSmokeTests(unittest.TestCase):
    class Clock:
        def __init__(self):
            self.now = 10

        def monotonic(self):
            return self.now

        def sleep(self, seconds):
            self.now += seconds

    def harness(self, *, buttons=True, launched=True, visible=True, alive=True, early_code=None):
        process = Mock(pid=123, returncode=early_code)
        process.poll.side_effect = lambda: process.returncode
        def wait(timeout):
            if process.returncode is None:
                raise subprocess.TimeoutExpired("owned fixture installer", timeout)
            return process.returncode
        process.wait.side_effect = wait
        process.terminate.side_effect = lambda: setattr(process, "returncode", -1)
        application = Mock(identity=(789, 987654321))
        application.visible.return_value = visible
        application.alive.return_value = alive
        ui = Mock()
        ui.button.side_effect = lambda pid, caption: (pid, caption) if buttons else None
        def click(button):
            if button == (123, "Close"):
                process.returncode = 0
        ui.click.side_effect = click
        ui.application.side_effect = lambda _executable, **_options: application if launched and process.returncode == 0 else None
        ui.applications.side_effect = lambda _executable: [application] if launched and process.returncode == 0 else []
        return process, application, ui

    def execute(self, process, ui, **options):
        environment = {"FIXTURE": "sanitized"}
        clock = self.Clock()
        with patch.object(release, "WindowsSmokeUI", return_value=ui), patch.object(release.subprocess, "Popen", return_value=process) as popen, patch.object(release.time, "monotonic", clock.monotonic), patch.object(release.time, "sleep", clock.sleep):
            release.interactive_installer_smoke(Path("C:/setup with spaces.exe"), Path("C:/private/install with spaces"), Path("C:/private/install with spaces/versions/0.1.2-12/maple-agent.exe"), Path("C:/private"), environment, **options)
        return popen, environment, clock

    def test_drives_only_owned_installer_and_requires_its_actual_visible_auto_launch(self):
        process, application, ui = self.harness()
        popen, environment, clock = self.execute(process, ui)
        command = popen.call_args.args[0]
        self.assertEqual(command, '"C:/setup with spaces.exe" /D=C:/private/install with spaces')
        self.assertNotIn("/S", command)
        self.assertIs(popen.call_args.kwargs["env"], environment)
        self.assertEqual(popen.call_args.kwargs["cwd"], Path("C:/private"))
        self.assertEqual(ui.button.call_args_list, [unittest.mock.call(123, "Install"), unittest.mock.call(123, "Close")])
        self.assertEqual(ui.click.call_args_list, [unittest.mock.call((123, "Install")), unittest.mock.call((123, "Close"))])
        self.assertGreaterEqual(clock.now, 11)
        application.visible.assert_called()
        application.close.assert_called_once()
        process.terminate.assert_not_called()

    def test_actual_installer_failure_is_preserved_and_never_launches_a_fallback_app(self):
        process, application, ui = self.harness(early_code=41)
        with self.assertRaises(subprocess.CalledProcessError) as raised:
            self.execute(process, ui)
        self.assertEqual(raised.exception.returncode, 41)
        ui.click.assert_not_called()
        application.close.assert_not_called()
        process.terminate.assert_not_called()

    def test_legacy_seed_runs_after_install_before_close_and_gui_waits_for_real_metadata(self):
        process, application, ui = self.harness()
        def seed(timeout):
            self.assertGreater(timeout, 0)
            self.assertIsNone(process.returncode)
            self.assertEqual(ui.click.call_args_list, [unittest.mock.call((123, "Install"))])
        before_close = Mock(side_effect=seed)
        integration_ready = Mock(side_effect=[False, False, True])
        self.execute(process, ui, before_close=before_close, integration_ready=integration_ready)
        before_close.assert_called_once()
        self.assertEqual(integration_ready.call_count, 3)
        application.close.assert_called_once()

    def test_visible_app_with_unfinished_icon_migration_cannot_pass(self):
        process, application, ui = self.harness()
        integration_ready = Mock(return_value=False)
        with self.assertRaisesRegex(ValueError, "shell icon refresh did not finish"):
            self.execute(process, ui, integration_ready=integration_ready)
        integration_ready.assert_called()
        application.close.assert_called_once()

    def test_auto_launched_gui_must_remain_live_during_metadata_inspection(self):
        process, application, ui = self.harness()
        def ready(_timeout):
            application.alive.return_value = False
            return True
        with self.assertRaisesRegex(ValueError, "during icon verification"):
            self.execute(process, ui, integration_ready=ready)
        application.close.assert_called_once()

    def test_missing_install_ui_times_out_and_stops_only_the_owned_installer(self):
        process, application, ui = self.harness(buttons=False)
        with self.assertRaisesRegex(ValueError, "did not reach Install"):
            self.execute(process, ui)
        ui.click.assert_not_called()
        process.terminate.assert_called_once()
        application.close.assert_not_called()

    def test_launch_before_close_is_a_failure_and_its_exact_process_is_cleaned_up(self):
        process, application, ui = self.harness()
        ui.application.return_value = application
        ui.application.side_effect = None
        with self.assertRaisesRegex(ValueError, "before it closed"):
            self.execute(process, ui)
        self.assertEqual(ui.click.call_args_list, [unittest.mock.call((123, "Install"))])
        process.terminate.assert_called_once()
        application.close.assert_called_once()

    def test_success_without_an_auto_launch_or_visible_window_cannot_pass(self):
        for changes in ({"launched": False}, {"visible": False}, {"alive": False}):
            with self.subTest(changes=changes):
                process, application, ui = self.harness(**changes)
                with self.assertRaisesRegex(ValueError, "did not open|exited early"):
                    self.execute(process, ui)
                if changes.get("launched", True):
                    application.close.assert_called_once()
                process.terminate.assert_not_called()

    def test_cleanup_failure_does_not_hide_the_primary_or_prevent_owned_app_cleanup(self):
        process, application, ui = self.harness(alive=False)
        process.wait.side_effect = [0, OSError("private cleanup details")]
        diagnostics = io.StringIO()
        with patch.object(sys, "stderr", diagnostics), self.assertRaisesRegex(ValueError, "exited early"):
            self.execute(process, ui)
        application.close.assert_called_once()
        self.assertIn("Windows interactive smoke cleanup failure: OSError", diagnostics.getvalue())
        self.assertNotIn("private cleanup details", diagnostics.getvalue())

    def test_failed_hidden_launch_cleans_all_new_processes_and_preserves_the_existing_host(self):
        process, _, ui = self.harness(launched=False)
        existing = Mock(identity=(456, 111))
        hidden_gui = Mock(identity=(789, 222))
        new_host = Mock(identity=(987, 333))
        ui.applications.side_effect = None
        ui.applications.return_value = [existing, hidden_gui, new_host]
        clock = self.Clock()
        with patch.object(release.subprocess, "Popen", return_value=process), patch.object(release.time, "monotonic", clock.monotonic), patch.object(release.time, "sleep", clock.sleep):
            with self.assertRaisesRegex(ValueError, "did not open"):
                release.interactive_installer_smoke(Path("fixture.exe"), Path("private/install"), Path("private/generation/maple-agent.exe"), Path("private"), {}, ui=ui, existing={(456, 111)})
        existing.release.assert_called_once()
        existing.close.assert_not_called()
        hidden_gui.close.assert_called_once()
        new_host.close.assert_called_once()

    def test_silent_install_must_not_start_even_a_hidden_generation_process(self):
        _, application, ui = self.harness()
        application.identity = (123, 456)
        ui.applications.side_effect = None
        ui.applications.return_value = [application]
        with patch.object(release, "WindowsSmokeUI", return_value=ui), self.assertRaisesRegex(ValueError, "Silent Windows installer launched"):
            release.quiet_installer_smoke(Path("C:/private/generation/maple-agent.exe"))
        application.visible.assert_not_called()
        application.close.assert_called_once()

    def test_silent_repeat_preserves_existing_host_and_checks_for_new_processes(self):
        _, application, ui = self.harness()
        application.identity = (123, 456)
        ui.applications.side_effect = None
        ui.applications.return_value = [application]
        clock = self.Clock()
        with patch.object(release.time, "monotonic", clock.monotonic), patch.object(release.time, "sleep", clock.sleep):
            release.quiet_installer_smoke(Path("C:/private/generation/maple-agent.exe"), ui=ui, existing={(123, 456)})
        application.release.assert_called()
        application.close.assert_not_called()
        self.assertGreaterEqual(clock.now, 11)

    def test_existing_host_handles_are_retained_and_released_without_stopping_them(self):
        host = Mock(identity=(456, 987654321))
        ui = Mock()
        ui.applications.return_value = [host]
        executable = Path("C:/private/generation/maple-agent.exe")
        with patch.object(release, "WindowsSmokeUI", return_value=ui):
            with release.smoke_application_baseline(executable) as (driver, existing):
                self.assertIs(driver, ui)
                self.assertEqual(existing, {(456, 987654321)})
                host.release.assert_not_called()
            ui.applications.assert_called_once_with(executable)
        host.release.assert_called_once()
        host.close.assert_not_called()

    def test_gui_selection_skips_existing_host_and_scans_all_matching_processes(self):
        ui = object.__new__(release.WindowsSmokeUI)
        host = Mock(identity=(456, 111))
        hidden = Mock(identity=(789, 222))
        hidden.visible.return_value = False
        gui = Mock(identity=(456, 333))  # A reused PID is a different identity.
        gui.visible.return_value = True
        ui.applications = Mock(return_value=[host, hidden, gui])
        selected = ui.application(Path("C:/private/generation/maple-agent.exe"), existing={(456, 111)}, visible_only=True)
        self.assertIs(selected, gui)
        host.visible.assert_not_called()
        host.release.assert_called_once()
        hidden.release.assert_called_once()
        gui.release.assert_not_called()

    def test_owned_gui_exit_race_is_accepted_only_when_the_same_handle_has_exited(self):
        for exited in (True, False):
            with self.subTest(exited=exited):
                ui = Mock()
                ui.kernel.WaitForSingleObject.side_effect = [258, 0 if exited else 258, 0]
                ui.kernel.TerminateProcess.return_value = 0
                primary = OSError("fixed native failure")
                ui.ctypes.WinError.return_value = primary
                application = object.__new__(release.WindowsSmokeApplication)
                application.ui, application.handle = ui, 987
                if exited:
                    application.close()
                else:
                    with self.assertRaises(OSError) as raised:
                        application.close()
                    self.assertIs(raised.exception, primary)
                ui.kernel.TerminateProcess.assert_called_once_with(987, 1)
                ui.kernel.CloseHandle.assert_called_once_with(987)


class NativeShortcutProbeTests(unittest.TestCase):
    def native(self, metadata):
        native = object.__new__(release.WindowsShortcutCOM)
        native.ctypes, native.hresult = ctypes, ctypes.c_int32
        native.guid_type = ctypes.c_ubyte * 16
        native.guid = lambda _value: native.guid_type()
        native.phase, native.release = Mock(), Mock()
        def create(_clsid, _outer, _context, _iid, output):
            ctypes.cast(output, ctypes.POINTER(ctypes.c_void_p)).contents.value = 111
            return 0
        native.ole = types.SimpleNamespace(CoCreateInstance=Mock(side_effect=create))
        def call(_interface, slot, *arguments, **_options):
            if slot == 0:
                ctypes.cast(arguments[1], ctypes.POINTER(ctypes.c_void_p)).contents.value = 222
        native.call = Mock(side_effect=call)
        native.metadata = Mock(return_value=metadata)
        return native

    def test_native_seed_saves_only_after_raw_target_and_empty_argument_guards(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "owned.lnk"
            path.write_bytes(b"native link fixture")
            target = Path(temporary) / "maple-agent.exe"
            for changed in ({}, {"target": "C:/other.exe"}, {"arguments": "--custom"}):
                with self.subTest(changed=changed):
                    native = self.native({"target": str(target), "arguments": "", **changed})
                    with patch.object(release, "windows_shortcut_target_equal", side_effect=release.windows_path_equal):
                        if changed:
                            with self.assertRaisesRegex(ValueError, "not owned"):
                                native.probe(path, target, seed=True, working_directory=Path(temporary))
                        else:
                            native.probe(path, target, seed=True, working_directory=Path(temporary))
                    slots = [call.args[1] for call in native.call.call_args_list]
                    self.assertEqual(slots, [0, 5] if changed else [0, 5, 17, 9, 7, 6])
                    self.assertEqual(native.call.call_args_list[1].args[2:], (str(path), 0))
                    self.assertEqual([call.args[0].value for call in native.release.call_args_list], [222, 111])

    def test_native_read_is_data_only_and_releases_both_interfaces_after_read_failure(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "owned.lnk"
            path.write_bytes(b"native link fixture")
            native = self.native({"target": "C:/owned.exe", "arguments": ""})
            with patch.object(release, "windows_shortcut_target_equal", return_value=True):
                native.probe(path, Path("C:/owned.exe"))
            self.assertEqual([call.args[1] for call in native.call.call_args_list], [0, 5])
            primary = OSError("fixed native read failure")
            native.metadata.side_effect = primary
            with self.assertRaises(OSError) as raised:
                native.probe(path, Path("C:/owned.exe"))
            self.assertIs(raised.exception, primary)
            self.assertEqual(native.release.call_count, 4)

    def test_sta_success_and_already_initialized_balance_uninitialize_but_failed_init_does_not(self):
        for status in (0, 1, 0x80010106):
            with self.subTest(status=status):
                ole = types.SimpleNamespace(CoInitializeEx=Mock(return_value=status), CoUninitialize=Mock(), CoCreateInstance=Mock(), CoTaskMemFree=Mock())
                with patch.object(ctypes, "WinDLL", return_value=ole, create=True):
                    native = release.WindowsShortcutCOM()
                    self.assertEqual(ctypes.sizeof(native.guid_type), 16)
                    self.assertEqual(bytes(native.guid("000214f9-0000-0000-c000-000000000046")), bytes.fromhex("f914020000000000c000000000000046"))
                    if status < 2:
                        with native:
                            pass
                        ole.CoUninitialize.assert_called_once()
                    else:
                        with self.assertRaisesRegex(OSError, "HRESULT 0x80010106"):
                            with native:
                                self.fail("Changed COM apartment cannot admit the shortcut probe")
                        ole.CoUninitialize.assert_not_called()
                ole.CoInitializeEx.assert_called_once_with(None, 2)

    def test_public_native_probe_refuses_link_or_unicode_inputs_before_opening_com(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            path = directory / "owned.lnk"
            path.write_bytes(b"fixture")
            link = directory / "alias.lnk"
            link.symlink_to(path)
            with patch.object(release, "WindowsShortcutCOM") as native:
                with self.assertRaisesRegex(ValueError, "regular files"):
                    release.probe_windows_shortcut(link, path)
                with self.assertRaisesRegex(ValueError, "Unicode bound"):
                    release.probe_windows_shortcut(path, path, description="bad\0value")
                native.assert_not_called()

    def test_known_folder_frees_the_returned_allocation_even_when_hresult_fails(self):
        native = Mock(ctypes=ctypes, hresult=ctypes.c_int32, guid_type=ctypes.c_ubyte * 16)
        native.guid.return_value = native.guid_type()
        primary = OSError("fixed known-folder failure")
        native.check.side_effect = primary
        def locate(_folder, _flags, _token, output):
            ctypes.cast(output, ctypes.POINTER(ctypes.c_void_p)).contents.value = 987
            return -1
        shell = types.SimpleNamespace(SHGetKnownFolderPath=Mock(side_effect=locate))
        with patch.object(release, "WindowsShortcutCOM") as context, patch.object(ctypes, "WinDLL", return_value=shell, create=True):
            context.return_value.__enter__.return_value = native
            with self.assertRaises(OSError) as raised:
                release.windows_programs_directory()
        self.assertIs(raised.exception, primary)
        self.assertEqual(native.ole.CoTaskMemFree.call_args.args[0].value, 987)


class ShortcutTargetIdentityTests(unittest.TestCase):
    def test_short_and_long_spellings_match_only_after_strict_canonical_paths_match(self):
        short = r"C:\Users\RUNNER~1\Temp\private\maple-agent.exe"
        long = r"C:\Users\runneradmin\Temp\private\maple-agent.exe"
        final = Path(r"\\?\C:\Users\runneradmin\Temp\private\maple-agent.exe")
        with patch.object(release, "_canonical_shortcut_targets", return_value=(final, Path(long))) as canonical:
            self.assertTrue(release.windows_shortcut_target_equal(short, long))
            canonical.assert_called_once_with(Path(short), Path(long))
        with patch.object(release, "_canonical_shortcut_targets", return_value=(final, Path(long.replace("private", "foreign")))):
            self.assertFalse(release.windows_shortcut_target_equal(short, long))

    def test_nonlocal_device_stream_and_ambiguous_paths_are_refused_before_filesystem_access(self):
        bad = (r"\\server\share\maple-agent.exe", r"\\.\C:\maple-agent.exe",
               r"\\?\UNC\server\share\maple-agent.exe", r"C:maple-agent.exe", r"relative\maple-agent.exe",
               r"C:\private\maple-agent.exe:alternate", r"C:\private\..\maple-agent.exe",
               "C:\\private.\\maple-agent.exe", "C:\\private \\maple-agent.exe", r"C:\private\NUL.exe")
        with patch.object(release, "_canonical_shortcut_targets") as canonical:
            for value in bad:
                with self.subTest(value=value), self.assertRaises(ValueError):
                    release.windows_shortcut_target_equal(value, r"C:\private\maple-agent.exe")
            canonical.assert_not_called()

    def test_canonicalization_refuses_leaf_links_and_checks_both_regular_paths_first(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            target = root / "maple-agent.exe"
            target.write_bytes(b"ordinary fixture")
            linked = root / "linked.exe"
            linked.symlink_to(target)
            with patch.object(Path, "resolve") as resolve:
                for actual, expected in ((linked, target), (target, linked), (target, root / "missing.exe")):
                    with self.subTest(actual=actual, expected=expected), self.assertRaises(ValueError):
                        release._canonical_shortcut_targets(actual, expected)
                resolve.assert_not_called()

    def test_regular_reparse_ancestor_is_refused_before_strict_canonicalization(self):
        actual, expected = Path("C:/private/maple-agent.exe"), Path("C:/expected/maple-agent.exe")
        guarded = []
        def regular(path):
            guarded.append(path)
            if path == expected:
                raise ValueError("fixed reparse ancestor refusal")
        with patch.object(release, "regular", side_effect=regular), patch.object(Path, "resolve") as resolve:
            with self.assertRaisesRegex(ValueError, "reparse ancestor"):
                release._canonical_shortcut_targets(actual, expected)
        self.assertEqual(guarded, [actual, expected])
        resolve.assert_not_called()

    def test_foreign_hardlink_name_is_not_the_same_canonical_target(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            expected = root / "maple-agent.exe"
            expected.write_bytes(b"same bytes and inode do not grant target ownership")
            foreign = root / "foreign.exe"
            foreign.hardlink_to(expected)
            self.assertTrue(expected.samefile(foreign))
            left, right = release._canonical_shortcut_targets(foreign, expected)
            self.assertNotEqual(left, right)
            left, right = release._canonical_shortcut_targets(expected, expected)
            self.assertEqual(left, right)

    def test_canonicalization_is_strict_and_does_not_rewrite_raw_shortcut_metadata(self):
        actual, expected = Mock(), Mock()
        actual.resolve.return_value, expected.resolve.return_value = Path("same"), Path("same")
        with patch.object(release, "regular") as regular:
            self.assertEqual(release._canonical_shortcut_targets(actual, expected), (Path("same"), Path("same")))
        self.assertEqual(regular.call_args_list, [unittest.mock.call(actual), unittest.mock.call(expected)])
        actual.resolve.assert_called_once_with(strict=True)
        expected.resolve.assert_called_once_with(strict=True)
        metadata = {"target": r"C:\Users\RUNNER~1\private\maple-agent.exe", "arguments": ""}
        native = NativeShortcutProbeTests().native(metadata)
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "owned.lnk"
            path.write_bytes(b"fixture")
            with patch.object(release, "windows_shortcut_target_equal", return_value=True):
                result = native.probe(path, Path(r"C:\Users\runneradmin\private\maple-agent.exe"))
        self.assertIs(result, metadata)
        self.assertIn("RUNNER~1", result["target"])

    def test_ownership_mismatch_reports_bounded_raw_and_expected_targets_without_argument_contents(self):
        native = NativeShortcutProbeTests().native({"target": "C:/" + "a" * 2000, "arguments": "private argument fixture"})
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "owned.lnk"
            path.write_bytes(b"fixture")
            expected = Path("C:/expected/maple-agent.exe")
            with self.assertRaises(ValueError) as raised:
                native.probe(path, expected, seed=True)
        message = str(raised.exception)
        self.assertIn("target=", message)
        self.assertIn("expected=" + repr(str(expected)), message)
        self.assertIn("arguments_empty=False", message)
        self.assertNotIn("private argument fixture", message)
        self.assertLess(len(message), 750)


class WindowsIntegrationProbeTests(unittest.TestCase):
    def probe(self, scratch):
        installed = scratch / "install with spaces"
        registry = Mock(KEY_READ=1, KEY_WOW64_32KEY=2, KEY_SET_VALUE=4, HKEY_CURRENT_USER=5, REG_SZ=1)
        key = Mock()
        key.__enter__ = Mock(return_value=key)
        key.__exit__ = Mock(return_value=False)
        registry.OpenKey.return_value = key
        values = {"InstallLocation": str(installed), "DisplayIcon": f'"{installed / "maple-agent.exe"}",0'}
        registry.QueryValueEx.side_effect = lambda _key, name: (values[name], 1)
        registry.SetValueEx.side_effect = lambda _key, name, _reserved, _kind, value: values.update({name: value})
        with patch.dict(release.os.environ, {"SystemRoot": "C:/Windows"}):
            probe = release.WindowsIntegrationProbe(info(), installed, scratch, {"FIXTURE": "sanitized"}, registry)
        return probe, registry, values

    def metadata(self, probe, *, migrated=False):
        return {name: {
            "target": str(probe.installed / target), "arguments": "",
            "working_directory": str(probe.scratch), "description": "Maple icon migration smoke fixture",
            "hotkey": 0, "window_style": 1,
            "icon_location": f'{probe.icon if migrated else probe.installed / target},0',
        } for name, target in (("main", "maple-agent.exe"), ("uninstall", "uninstall.exe"))}

    def test_cache_path_and_bytes_are_derived_independently_from_canonical_profile_artwork(self):
        with tempfile.TemporaryDirectory() as temporary:
            scratch = Path(temporary)
            probe, _, _ = self.probe(scratch)
            artwork = (release.COMPONENT / "app/packaging/maple-agent-dev.ico").read_bytes()
            self.assertEqual(probe.artwork, artwork)
            self.assertEqual(probe.icon, scratch / "data" / info()["data_namespace"] / "windows-integration" / (hashlib.sha256(artwork).hexdigest() + ".ico"))
            self.assertFalse(probe.icon.is_relative_to(probe.installed))
            self.assertFalse(probe.icon.is_relative_to(scratch / "data" / info()["data_namespace"] / "store-v1"))

    def test_seed_uses_bounded_isolated_python_with_literal_arguments_for_fixed_owned_links(self):
        with tempfile.TemporaryDirectory(prefix="maple $literal ") as temporary:
            probe, registry, values = self.probe(Path(temporary))
            metadata = self.metadata(probe)
            result = subprocess.CompletedProcess("fixture probe", 0, json.dumps(metadata).encode())
            with patch.object(release, "capture_preflight", return_value=result) as capture:
                probe.seed()
            arguments, environment = capture.call_args.args
            self.assertEqual(arguments[:3], [sys.executable, "-I", "-c"])
            self.assertEqual(arguments[4:], [Path(release.__file__).resolve(), info()["display_name"], probe.installed, probe.scratch, "seed", probe.checkpoint])
            self.assertEqual(environment, {"FIXTURE": "sanitized"})
            self.assertEqual(capture.call_args.kwargs["timeout"], 20)
            self.assertEqual(probe.before, metadata)
            self.assertEqual(values["DisplayIcon"], f'"{probe.installed / "maple-agent.exe"}",0')
            registry.SetValueEx.assert_called_once()

    def test_exact_python_child_protocol_runs_with_private_fixture_paths_and_native_api_contract(self):
        with tempfile.TemporaryDirectory(prefix="maple $literal ") as temporary:
            probe, _, _ = self.probe(Path(temporary))
            probe.probe_module = Path(temporary) / "native fixture $literal.py"
            probe.probe_module.write_text('''from pathlib import Path
def windows_programs_directory():
    return Path(__file__).parent / "Programs"
def probe_windows_shortcut(path, expected_target, *, seed, working_directory, phase):
    phase("link-save" if seed else "link-read")
    return {"target": str(expected_target), "arguments": "", "working_directory": str(working_directory),
            "description": "fixture", "hotkey": 0, "window_style": 3, "icon_location": str(path) + ",0"}
''')
            actual = probe.shortcuts(seed=True)
            self.assertEqual(actual["main"]["target"], str(probe.installed / "maple-agent.exe"))
            self.assertEqual(actual["uninstall"]["target"], str(probe.installed / "uninstall.exe"))
            self.assertEqual(actual["main"]["working_directory"], str(probe.scratch))
            self.assertEqual(actual["main"]["window_style"], 3)
            self.assertEqual(probe.checkpoint.read_text(), "link-save")

    def test_native_child_timeout_reports_only_the_fixed_last_phase_and_preserves_failure(self):
        with tempfile.TemporaryDirectory() as temporary:
            probe, _, _ = self.probe(Path(temporary))
            primary = subprocess.TimeoutExpired("fixed child", 20)
            def timeout(*_arguments, **_options):
                probe.checkpoint.write_text("link-load")
                raise primary
            diagnostics = io.StringIO()
            with patch.object(release, "capture_preflight", side_effect=timeout), patch.object(sys, "stderr", diagnostics):
                with self.assertRaises(subprocess.TimeoutExpired) as raised:
                    probe.shortcuts(timeout=7)
            self.assertIs(raised.exception, primary)
            self.assertEqual(diagnostics.getvalue(), "Windows shortcut probe stopped at: link-load\n")

    def test_seed_refuses_a_changed_registered_root_before_any_shortcut_or_registry_mutation(self):
        with tempfile.TemporaryDirectory() as temporary:
            probe, registry, values = self.probe(Path(temporary))
            values["InstallLocation"] = "C:/another installation"
            with patch.object(release, "capture_preflight") as capture, self.assertRaisesRegex(ValueError, "changed installation root"):
                probe.seed()
            capture.assert_not_called()
            registry.SetValueEx.assert_not_called()

    def test_old_display_metadata_is_pending_and_does_not_probe_shortcuts_or_an_arbitrary_image(self):
        with tempfile.TemporaryDirectory() as temporary:
            probe, _, values = self.probe(Path(temporary))
            probe.before = self.metadata(probe)
            for value in (values["DisplayIcon"], '"C:/unrelated/icon.ico",0'):
                values["DisplayIcon"] = value
                with patch.object(probe, "shortcuts") as shortcuts:
                    self.assertFalse(probe.ready())
                shortcuts.assert_not_called()

    def test_final_metadata_requires_exact_canonical_file_and_preserves_all_other_link_properties(self):
        with tempfile.TemporaryDirectory() as temporary:
            probe, _, values = self.probe(Path(temporary))
            probe.before = self.metadata(probe)
            values["DisplayIcon"] = f'"{probe.icon}",0'
            probe.icon.parent.mkdir(parents=True)
            probe.icon.write_bytes(probe.artwork)
            actual = self.metadata(probe, migrated=True)
            with patch.object(probe, "shortcuts", return_value=actual) as shortcuts:
                self.assertTrue(probe.ready(timeout=7))
            shortcuts.assert_called_once_with(timeout=7)
            for field in ("target", "arguments", "working_directory", "description", "hotkey", "window_style"):
                with self.subTest(field=field):
                    changed = json.loads(json.dumps(actual))
                    changed["main"][field] = "changed"
                    with patch.object(probe, "shortcuts", return_value=changed), self.assertRaisesRegex(ValueError, "non-icon"):
                        probe.ready()
            actual["uninstall"]["icon_location"] = str(probe.installed / "versions/old/maple-agent.exe") + ",0"
            with patch.object(probe, "shortcuts", return_value=actual), self.assertRaisesRegex(ValueError, "persistent profile"):
                probe.ready()
            probe.icon.write_bytes(b"wrong artwork")
            with self.assertRaisesRegex(ValueError, "canonical profile artwork"):
                probe.ready()

    def test_link_probe_rejects_missing_extra_or_duplicate_native_metadata(self):
        with tempfile.TemporaryDirectory() as temporary:
            probe, _, _ = self.probe(Path(temporary))
            for output in (b'{}', b'{"main":{},"uninstall":{}}', b'{"main":{},"main":{},"uninstall":{}}'):
                result = subprocess.CompletedProcess("fixture probe", 0, output)
                with self.subTest(output=output), patch.object(release, "capture_preflight", return_value=result), self.assertRaises(ValueError):
                    probe.shortcuts()

    def test_icon_reference_accepts_only_the_expected_file_and_zero_index(self):
        path = Path("C:/Maple icons/canonical.ico")
        for value in ('"C:\\Maple icons\\canonical.ico",0', 'c:/maple icons/canonical.ico, 0'):
            self.assertTrue(release.windows_icon_equal(value, path))
        for value in (None, str(path), str(path) + ",1", str(path) + ",0,0", '"C:/other.ico",0'):
            self.assertFalse(release.windows_icon_equal(value, path))

    def test_gui_requires_first_render_and_lifetime_before_integration_readiness(self):
        for rendered in (False, True):
            with self.subTest(rendered=rendered), tempfile.TemporaryDirectory() as temporary:
                clock = InteractiveInstallerSmokeTests.Clock()
                process = Mock()
                process.poll.return_value = None
                def spawn(_arguments, **options):
                    if rendered:
                        options["stderr"].write(b"startup: window open at 0\nstartup: first render at 0\n")
                        options["stderr"].flush()
                    return process
                ready = Mock(side_effect=[False, True])
                with patch.object(release.subprocess, "Popen", side_effect=spawn), patch.object(release.time, "monotonic", clock.monotonic), patch.object(release.time, "sleep", clock.sleep), patch.object(sys, "stderr", io.StringIO()):
                    if rendered:
                        release.gui_smoke(Path("private/generation.exe"), Path(temporary), {}, integration_ready=ready)
                        self.assertEqual(ready.call_count, 2)
                        self.assertGreaterEqual(clock.now, 11)
                    else:
                        with self.assertRaisesRegex(ValueError, "create and render"):
                            release.gui_smoke(Path("private/generation.exe"), Path(temporary), {}, integration_ready=ready)
                        ready.assert_not_called()
                process.terminate.assert_called_once()

    def test_rendered_gui_cannot_exit_during_metadata_probe_and_still_pass(self):
        with tempfile.TemporaryDirectory() as temporary:
            clock = InteractiveInstallerSmokeTests.Clock()
            process = Mock(returncode=None)
            process.poll.side_effect = lambda: process.returncode
            def spawn(_arguments, **options):
                options["stderr"].write(b"startup: window open at 0\nstartup: first render at 0\n")
                options["stderr"].flush()
                return process
            def ready(_timeout):
                process.returncode = 1
                return True
            with patch.object(release.subprocess, "Popen", side_effect=spawn), patch.object(release.time, "monotonic", clock.monotonic), patch.object(release.time, "sleep", clock.sleep), patch.object(sys, "stderr", io.StringIO()):
                with self.assertRaisesRegex(ValueError, "during icon verification"):
                    release.gui_smoke(Path("private/generation.exe"), Path(temporary), {}, integration_ready=ready)
            process.terminate.assert_not_called()


class PointerPinTests(unittest.TestCase):
    def kernel(self, handle=123):
        return types.SimpleNamespace(CreateFileW=Mock(return_value=handle), CloseHandle=Mock(return_value=1))

    def test_pin_requests_explicit_read_sharing_and_closes_after_its_body(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "current.json"
            path.write_bytes(b"fixed pointer fixture")
            kernel = self.kernel()
            with patch.object(ctypes, "WinDLL", return_value=kernel, create=True) as library:
                with release.pin_current_pointer(path):
                    library.assert_called_once_with("kernel32", use_last_error=True)
                    kernel.CreateFileW.assert_called_once_with(str(path), 0x80000000, 1, None, 3, 0, None)
                    self.assertEqual(len(kernel.CreateFileW.argtypes), 7)
                    self.assertIsNotNone(kernel.CreateFileW.restype)
                    self.assertEqual(path.read_bytes(), b"fixed pointer fixture")
                    kernel.CloseHandle.assert_not_called()
                kernel.CloseHandle.assert_called_once_with(123)

    def test_pin_open_failure_does_not_run_the_body_or_close_an_invalid_handle(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "current.json"
            path.write_bytes(b"fixed pointer fixture")
            kernel = self.kernel(ctypes.c_void_p(-1).value)
            primary = OSError("fixed native open failure")
            with patch.object(ctypes, "WinDLL", return_value=kernel, create=True), patch.object(ctypes, "get_last_error", return_value=5, create=True), patch.object(ctypes, "WinError", return_value=primary, create=True):
                with self.assertRaises(OSError) as raised:
                    with release.pin_current_pointer(path):
                        self.fail("A failed pointer pin must not admit repeat installation")
            self.assertIs(raised.exception, primary)
            kernel.CloseHandle.assert_not_called()

    def test_pin_close_failure_preserves_primary_installer_error_and_surfaces_without_one(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "current.json"
            path.write_bytes(b"fixed pointer fixture")
            kernel = self.kernel()
            kernel.CloseHandle.return_value = 0
            secondary = OSError("do not expose close details")
            primary = subprocess.CalledProcessError(0x4D090020, "fixture installer")
            diagnostics = io.StringIO()
            with patch.object(ctypes, "WinDLL", return_value=kernel, create=True), patch.object(ctypes, "get_last_error", return_value=5, create=True), patch.object(ctypes, "WinError", return_value=secondary, create=True), patch.object(sys, "stderr", diagnostics):
                with self.assertRaises(subprocess.CalledProcessError) as raised:
                    with release.pin_current_pointer(path):
                        raise primary
                self.assertIs(raised.exception, primary)
                with self.assertRaises(OSError) as raised:
                    with release.pin_current_pointer(path):
                        pass
                self.assertIs(raised.exception, secondary)
            self.assertEqual(kernel.CloseHandle.call_count, 2)
            self.assertIn("Windows pointer pin cleanup failure: OSError", diagnostics.getvalue())
            self.assertNotIn("do not expose", diagnostics.getvalue())

    def test_actual_repeat_and_identity_assertions_are_the_only_pinned_smoke_operations(self):
        # Portable scope contract; the actual sharing denial is Windows-only.
        tree = ast.parse((SCRIPTS / "windows-release.py").read_text())
        smoke = next(node for node in tree.body if isinstance(node, ast.FunctionDef) and node.name == "smoke_installer")
        pinned = [node for node in ast.walk(smoke) if isinstance(node, ast.With) and
                  any(isinstance(item.context_expr, ast.Call) and isinstance(item.context_expr.func, ast.Name) and
                      item.context_expr.func.id == "pin_current_pointer" for item in node.items)]
        self.assertEqual(len(pinned), 1)
        self.assertEqual([type(node) for node in pinned[0].body], [ast.Expr, ast.If])
        self.assertEqual(pinned[0].body[0].value.func.id, "run")
        self.assertEqual(pinned[0].body[1].body[0].exc.func.id, "ValueError")
        self.assertIn("current.json", ast.unparse(pinned[0]))
        self.assertIn("bootstrap_hash", ast.unparse(pinned[0]))


class SmokeDiagnosticsTests(unittest.TestCase):
    def registry(self):
        def missing(*_arguments):
            raise FileNotFoundError
        return types.SimpleNamespace(KEY_READ=1, KEY_WOW64_32KEY=2, HKEY_CURRENT_USER=3, OpenKey=missing)

    def test_native_activation_status_is_bounded_to_known_stages_and_marker(self):
        self.assertEqual(set(release.ACTIVATION_FAILURE_STAGES), set(range(1, 11)))
        for stage, name in release.ACTIVATION_FAILURE_STAGES.items():
            for raw in (0, 5, 32, 65535):
                with self.subTest(stage=stage, raw=raw):
                    code = 0x4D000000 | (stage << 16) | raw
                    self.assertEqual(release.decode_activation_failure(code), {
                        "native_stage": name, "native_win32_error": raw or None,
                    })
        for code in (None, True, "1292435488", -1, 0, 1, 40, 41, 0xC0000005,
                     0x4D000005, 0x4D0B0005, 0x4DFF0005, 0x14D090005):
            with self.subTest(code=code):
                self.assertIsNone(release.decode_activation_failure(code))

    def test_nsis_actual_numeric_branch_passes_only_known_activation_failures(self):
        # Interpret only the wrapper's small numeric branch, not NSIS/runtime
        # behavior. This catches decimal-vs-hex string comparisons and jumps
        # that would lose or admit a native status before the Windows rehearsal.
        source = (SCRIPTS / "windows-installer.nsi").read_text()
        branch = source.split("\nfailed:\n", 1)[1].split("\nactivation_diagnostic_done:\n", 1)[0]
        instructions = [line.strip() for line in branch.splitlines()
                        if line.strip() and not line.lstrip().startswith(";")]
        def propagated(phase, code):
            registers = {"$FailureCode": str(phase), "$NativeStatus": str(code)}
            index = 0
            def value(token):
                return registers.get(token, token)
            def number(token):
                return int(value(token), 0)
            while index < len(instructions):
                operation, *args = instructions[index].split()
                destination = "0"
                if operation == "StrCmp":
                    destination = args[2] if value(args[0]) == value(args[1]) else args[3]
                elif operation == "IntCmp":
                    left, right = number(args[0]), number(args[1])
                    destination = args[2 if left == right else 3 if left < right else 4]
                elif operation == "IntOp":
                    left, right = number(args[1]), number(args[3])
                    self.assertIn(args[2], ("&", ">>"))
                    registers[args[0]] = str(left & right if args[2] == "&" else left >> right)
                elif operation == "StrCpy":
                    registers[args[0]] = value(args[1])
                else:
                    self.fail("Unexpected instruction in activation diagnostic branch: " + operation)
                if destination != "0":
                    self.assertEqual(destination, "activation_diagnostic_done")
                    break
                index += 1
            return int(registers["$FailureCode"])
        for stage in range(1, 11):
            for raw in (0, 5, 32, 65535):
                code = 0x4D000000 | (stage << 16) | raw
                self.assertEqual(propagated(41, code), code)
                self.assertEqual(propagated(40, code), 40)
                self.assertEqual(propagated(30, code), 30)
        for code in (0, 1, -1, 0xC0000005, 0x4D000005, 0x4D0B0005, 0x4DFF0005):
            self.assertEqual(propagated(41, code), 41)

    def test_actual_child_status_is_reported_without_replay_or_primary_error_change(self):
        code = 0x4D090020
        primary = subprocess.CalledProcessError(code, "fixture installer")
        diagnostics = io.StringIO()
        with patch.dict(sys.modules, winreg=self.registry()), patch.object(release, "smoke_environment", return_value={}), patch.object(release, "capture_preflight", return_value=subprocess.CompletedProcess("checker", 0, b"")), patch.object(release, "run", side_effect=primary) as commands, patch.object(sys, "stderr", diagnostics):
            with self.assertRaises(subprocess.CalledProcessError) as raised:
                release.smoke_installer(Path("fixture.exe"), info(), False, "12", {}, uninstaller_sha256="a" * 64, checker=Path("verified-checker.exe"))
        self.assertIs(raised.exception, primary)
        self.assertEqual(commands.call_count, 1)
        evidence = json.loads(diagnostics.getvalue().split("Windows installer failure evidence: ", 1)[1])
        self.assertEqual(evidence["exit_code"], code)
        self.assertEqual(evidence["installer_phase"], "activation-child")
        self.assertEqual(evidence["native_stage"], "generation-guards")
        self.assertEqual(evidence["native_win32_error"], 32)

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
                    release.smoke_installer(Path("fixture.exe"), info(), False, "12", {}, uninstaller_sha256="a" * 64, checker=Path("verified-checker.exe"))
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
                release.smoke_installer(Path("fixture.exe"), info(), False, "12", {}, uninstaller_sha256="a" * 64, checker=Path("verified-checker.exe"))
        self.assertIs(raised.exception, primary)
        self.assertEqual(calls, 2)
        self.assertIn('"phase": "installed-verification"', diagnostics.getvalue())
        self.assertIn('"phase": "host-stop"', diagnostics.getvalue())

    def test_installed_verification_keeps_selected_architecture_and_primary_error(self):
        from contextlib import nullcontext
        from test_windows_build_info import icon_records, pe as executable_fixture

        for arch, machine, opposite in (("aarch64", 0xAA64, False), ("x86_64", 0x8664, False),
                                        ("aarch64", 0x8664, True)):
            with self.subTest(arch=arch, machine=machine), tempfile.TemporaryDirectory() as temporary:
                scratch = Path(temporary)
                identity = info("dev", arch)
                installed = scratch / "install with spaces"
                registry = self.registry()
                def open_key(*_arguments):
                    if not installed.exists():
                        raise FileNotFoundError
                    return nullcontext(object())
                registry.OpenKey = open_key
                registry.QueryValueEx = lambda *_args: (str(installed), 1)
                reached = ValueError("stop after real installed PE validation")
                original = release.verify_payload

                def install(command, **_options):
                    if not isinstance(command, str):
                        # Exercise primary-error preservation during host-stop
                        # cleanup, without starting a fixture executable.
                        raise PermissionError("secondary cleanup fixture")
                    generation = installed / "versions" / "0.1.2-12"
                    generation.mkdir(parents=True)
                    icon = release.COMPONENT / "app/packaging/maple-agent-dev.ico"
                    executable = executable_fixture(identity, machine=machine, resources=icon_records(icon.read_bytes()))
                    (generation / "maple-agent.exe").write_bytes(executable)
                    (generation / "build-info.json").write_text(json.dumps(identity))
                    (generation / "LICENSE.txt").write_bytes((release.COMPONENT / "LICENSE").read_bytes())
                    (installed / "maple-agent.exe").write_bytes(executable)
                    (installed / "current.json").write_text(json.dumps({"format": "maple-agent-windows-current-v1", "generation": "0.1.2-12"}))
                    return subprocess.CompletedProcess(command, 0)

                def installed_verification(*arguments, **options):
                    original(*arguments, **options)
                    raise reached

                diagnostics = io.StringIO()
                with patch.object(release, "verification_scratch", return_value=nullcontext(scratch)), patch.dict(sys.modules, winreg=registry), patch.object(release, "smoke_environment", return_value={}), patch.object(release, "source_sha", return_value="a" * 40), patch.object(release, "capture_preflight", return_value=subprocess.CompletedProcess("checker", 0, b"")), patch.object(release, "run", side_effect=install), patch.object(release, "verify_payload", side_effect=installed_verification) as verified, patch.object(release, "signature"), patch.object(sys, "stderr", diagnostics):
                    with self.assertRaises(ValueError) as raised:
                        release.smoke_installer(Path("fixture.exe"), identity, True, "12", {}, uninstaller_sha256="a" * 64, checker=Path("checker.exe"), arch=arch)
                self.assertEqual(verified.call_args.kwargs["arch"], arch)
                if opposite:
                    self.assertIn("PE architecture", str(raised.exception))
                else:
                    self.assertIs(raised.exception, reached)
                self.assertIn('"phase": "installed-verification"', diagnostics.getvalue())
                self.assertIn('"phase": "host-stop"', diagnostics.getvalue())
                self.assertNotIn("secondary cleanup fixture", diagnostics.getvalue())

    def test_failed_readonly_preflight_never_runs_the_installer_or_prepare(self):
        diagnostics = io.StringIO()
        result = subprocess.CompletedProcess(["checker", "--internal-windows-installer-preflight"], 1, b"fixed native guard failure\x1b[31m\x00")
        with patch.dict(sys.modules, winreg=self.registry()), patch.object(release, "smoke_environment", return_value={}), patch.object(release, "capture_preflight", return_value=result), patch.object(release, "run") as commands, patch.object(sys, "stderr", diagnostics):
            with self.assertRaises(subprocess.CalledProcessError):
                release.smoke_installer(Path("fixture.exe"), info(), False, "12", {}, uninstaller_sha256="a" * 64, checker=Path("verified-checker.exe"))
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
            release.smoke_installer(Path("preview.exe"), info("prod"), True, "12", {}, uninstaller_sha256="a" * 64, checker=Path("checker.exe"))
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
                release.smoke_installer(Path("preview.exe"), info("prod"), True, "12", {}, uninstaller_sha256="a" * 64, checker=Path("checker.exe"))


class CopiedUninstallerTests(unittest.TestCase):
    def fixture(self, scratch):
        installed = scratch / "install with spaces"
        installed.mkdir()
        uninstaller = installed / "uninstall.exe"
        uninstaller.write_bytes(b"exact verified NSIS fixture bytes")
        return installed, uninstaller, hashlib.sha256(uninstaller.read_bytes()).hexdigest()

    def test_copy_preserves_exact_verified_bytes_policy_and_waited_retirement(self):
        for unsigned in (True, False):
            with self.subTest(unsigned=unsigned), tempfile.TemporaryDirectory() as temporary:
                scratch = Path(temporary)
                installed, uninstaller, digest = self.fixture(scratch)
                copied = scratch / "uninstaller-runner/uninstall.exe"
                environment = {"FIXTURE": "isolated"}
                def uninstall(command, **options):
                    self.assertEqual(command, f'"{copied}" /S _?={installed}')
                    self.assertIs(options["env"], environment)
                    self.assertEqual(options["cwd"], scratch)
                    self.assertEqual(copied.read_bytes(), uninstaller.read_bytes())
                    self.assertEqual(hashlib.sha256(copied.read_bytes()).hexdigest(), digest)
                    # Model the actual child's completed work. A returned exit
                    # status cannot bless the original remaining on disk.
                    uninstaller.unlink()
                    return subprocess.CompletedProcess(command, 0)
                with patch.object(release, "signature") as signature, patch.object(release, "run", side_effect=uninstall):
                    release.run_copied_uninstaller(uninstaller, installed, scratch, environment, unsigned, digest, "aarch64")
                self.assertEqual(signature.call_args_list, [unittest.mock.call(uninstaller, unsigned), unittest.mock.call(copied, unsigned)])
                self.assertFalse(copied.parent.exists())
                self.assertFalse(uninstaller.exists())

    def test_wrong_source_hash_and_changed_verified_copy_never_execute(self):
        with tempfile.TemporaryDirectory() as temporary:
            scratch = Path(temporary)
            installed, uninstaller, digest = self.fixture(scratch)
            with patch.object(release, "signature"), patch.object(release, "run") as command:
                with self.assertRaisesRegex(ValueError, "verified installer"):
                    release.run_copied_uninstaller(uninstaller, installed, scratch, {}, True, "0" * 64, "aarch64")
                command.assert_not_called()
            def mutate_copy(path, _unsigned):
                if path.parent.name == "uninstaller-runner":
                    path.write_bytes(b"changed after signature inspection")
            with patch.object(release, "signature", side_effect=mutate_copy), patch.object(release, "run") as command:
                with self.assertRaisesRegex(ValueError, "changed during signature"):
                    release.run_copied_uninstaller(uninstaller, installed, scratch, {}, True, digest, "aarch64")
                command.assert_not_called()

    def test_original_remnant_and_denied_absence_probe_cannot_pass(self):
        for denied in (False, True):
            with self.subTest(denied=denied), tempfile.TemporaryDirectory() as temporary:
                scratch = Path(temporary)
                installed, uninstaller, digest = self.fixture(scratch)
                original_lstat = Path.lstat
                denial = PermissionError("fixed absence probe failure")
                # Set the denial only after the child returns; do not replace
                # the initial source regular-file/hash/signature admission.
                after_child = False
                def probe(path, *arguments, **options):
                    if denied and after_child and path == uninstaller:
                        raise denial
                    return original_lstat(path, *arguments, **options)
                def uninstall(command, **_options):
                    nonlocal after_child
                    after_child = True
                    return subprocess.CompletedProcess(command, 0)
                with patch.object(release, "signature"), patch.object(release, "run", side_effect=uninstall), patch.object(Path, "lstat", probe):
                    with self.assertRaises(PermissionError if denied else ValueError) as raised:
                        release.run_copied_uninstaller(uninstaller, installed, scratch, {}, True, digest, "aarch64")
                if denied:
                    self.assertIs(raised.exception, denial)
                else:
                    self.assertIn("original installed uninstaller", str(raised.exception))

    def test_missing_entry_is_success_but_access_denial_is_not_absence(self):
        with tempfile.TemporaryDirectory() as temporary:
            missing = Path(temporary) / "missing.exe"
            release.require_absent(missing, "must be missing")
            denial = PermissionError("fixed lstat denial")
            with patch.object(Path, "lstat", side_effect=denial), self.assertRaises(PermissionError) as raised:
                release.require_absent(missing, "must be missing")
            self.assertIs(raised.exception, denial)

    def test_child_failure_is_not_replaced_by_cleanup_or_retirement(self):
        with tempfile.TemporaryDirectory() as temporary:
            scratch = Path(temporary)
            installed, uninstaller, digest = self.fixture(scratch)
            primary = subprocess.CalledProcessError(52, "owned copied uninstaller")
            with patch.object(release, "signature"), patch.object(release, "run", side_effect=primary):
                with self.assertRaises(subprocess.CalledProcessError) as raised:
                    release.run_copied_uninstaller(uninstaller, installed, scratch, {}, True, digest, "aarch64")
            self.assertIs(raised.exception, primary)
            self.assertTrue(uninstaller.is_file())
            self.assertTrue((scratch / "uninstaller-runner/uninstall.exe").is_file())


@unittest.skipUnless(sys.platform == "win32", "requires the native Windows NSIS process model")
class NativeNSISUninstallCopyTests(unittest.TestCase):
    def test_verified_copy_retires_original_and_runner_after_real_child_exit(self):
        # Tiny pinned-tool fixture: this proves NSIS relocation/wait semantics,
        # not a Maple install, GUI, host, signed package or updater qualification.
        arch = release.os.environ.get("MAPLE_WINDOWS_ARCH", "x86_64")
        release.require_native_host(arch)
        with release.verification_scratch("maple-nsis-uninstall-fixture-") as scratch:
            compiler = release.install_nsis(scratch)
            source = scratch / "fixture.nsi"
            installer = scratch / "fixture-setup.exe"
            payload = scratch / "input.txt"
            payload.write_bytes(b"owned NSIS fixture payload")
            source.write_text(fr'''Unicode true
Name "Maple NSIS copy fixture"
OutFile "{release.nsis_literal(installer)}"
RequestExecutionLevel user
SilentInstall silent
SilentUnInstall silent
Section "Install"
  SetOutPath "$INSTDIR"
  File /oname=owned.txt "{release.nsis_literal(payload)}"
  WriteUninstaller "$INSTDIR\uninstall.exe"
  IfErrors failed
  SetErrorLevel 0
  Goto done
failed:
  SetErrorLevel 41
  Abort
done:
SectionEnd
Section "Uninstall"
  ClearErrors
  Delete "$INSTDIR\owned.txt"
  IfErrors failed
  Delete "$INSTDIR\uninstall.exe"
  IfErrors failed
  SetErrorLevel 0
  Goto done
failed:
  SetErrorLevel 51
  Abort
done:
SectionEnd
''', encoding="utf-8", newline="\n")
            release.run([compiler, "/V2", source], timeout=120)
            installed = scratch / "install with spaces"
            release.run(release.command_as_windows_args(f'"{installer}" /S /D={installed}'), timeout=120)
            uninstaller = installed / "uninstall.exe"
            digest = release.uninstaller_digest(uninstaller)
            sentinel = installed / "user-created-file.txt"
            sentinel.write_bytes(b"preserve this user file")
            environment = release.smoke_environment(scratch)
            release.run_copied_uninstaller(uninstaller, installed, scratch, environment, True, digest, arch)
            self.assertEqual({path.name for path in installed.iterdir()}, {sentinel.name})
            self.assertEqual(sentinel.read_bytes(), b"preserve this user file")
            self.assertFalse((scratch / "uninstaller-runner").exists())
        print("Verified pinned NSIS copied-uninstaller semantics only; no Maple runtime qualification", flush=True)


if __name__ == "__main__":
    unittest.main()
