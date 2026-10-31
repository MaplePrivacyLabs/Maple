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


class SuccessfulSmokeUninstallerLifecycleTests(unittest.TestCase):
    def exercise_success_cleanup(self, condition):
        from contextlib import ExitStack, nullcontext
        import shutil

        with tempfile.TemporaryDirectory() as temporary:
            scratch = (Path(temporary) / "scratch").resolve()
            scratch.mkdir()
            installed = scratch / "install with spaces"
            generation = installed / "versions" / "0.1.2-12"
            uninstaller = installed / "uninstall.exe"
            executable = installed / "maple-agent.exe"
            copied = scratch / "uninstaller-runner" / "uninstall.exe"
            identity = info("dev", "aarch64")
            payloads = {name: ("owned fixture " + name).encode() for name in release.BUNDLE_FILES}
            expected_hashes = {name: hashlib.sha256(data).hexdigest() for name, data in payloads.items()}
            uninstaller_bytes = b"verified owned installed uninstaller fixture"
            uninstaller_sha256 = hashlib.sha256(uninstaller_bytes).hexdigest()
            registry = SmokeDiagnosticsTests.registry(self)
            registered, finishing = False, False
            phases, cleanup_observations = [], []
            denied = PermissionError("fixed installed uninstaller access denial")
            original_regular = release.regular

            def open_key(*_arguments):
                if not registered:
                    raise FileNotFoundError
                return nullcontext(object())

            registry.OpenKey = open_key
            registry.QueryValueEx = lambda *_arguments: (str(installed), 1)

            def command(arguments, **_options):
                nonlocal registered
                if isinstance(arguments, str):
                    if not installed.exists():
                        self.assertEqual(Path(arguments.split("/D=", 1)[1]), installed)
                        generation.mkdir(parents=True)
                        for name, data in payloads.items():
                            (generation / name).write_bytes(data)
                        executable.write_bytes(payloads["maple-agent.exe"])
                        uninstaller.write_bytes(uninstaller_bytes)
                        (installed / "current.json").write_text(json.dumps({
                            "format": "maple-agent-windows-current-v1", "generation": "0.1.2-12",
                        }))
                        registered = True
                    return subprocess.CompletedProcess(arguments, 0)
                if list(arguments)[1:] == ["--build-info"]:
                    return subprocess.CompletedProcess(arguments, 0, stdout=json.dumps(identity))
                if list(arguments)[1:] == ["host", "stop"] and condition == "late-missing":
                    uninstaller.unlink()
                return subprocess.CompletedProcess(arguments, 0)

            def finish_interactive(*_arguments, **_options):
                nonlocal finishing
                finishing = True
                if condition in ("missing", "directory"):
                    uninstaller.unlink()
                    if condition == "directory":
                        uninstaller.mkdir()

            def require_regular(path):
                # Missing and directory cases exercise the real portable file
                # guard. Simulate Windows access denial at the same admission
                # boundary, without changing the machine's permissions.
                original_regular(path)
                if finishing and condition == "denied" and path == uninstaller:
                    raise denied

            def uninstall(source, root, owner, _environment, _unsigned, digest, _arch, **_options):
                nonlocal registered
                self.assertEqual((source, root, owner), (uninstaller, installed, scratch))
                self.assertEqual(release.uninstaller_digest(source), digest)
                self.assertEqual(digest, uninstaller_sha256)
                # Model product removal only. Standard temporary cleanup must
                # own the auxiliary runner, after real preservation assertions.
                copied.parent.mkdir()
                copied.write_bytes(source.read_bytes())
                executable.unlink()
                uninstaller.unlink()
                (installed / "current.json").unlink()
                shutil.rmtree(installed / "versions")
                registered = False

            def cleanup():
                if condition == "valid":
                    self.assertIn("uninstall-owned-files-and-state", phases)
                    self.assertEqual((installed / "user-created-file.txt").read_text(), "preserve me")
                    self.assertEqual((scratch / "data" / identity["data_namespace"] / "state-marker.txt").read_text(), "preserve state")
                    self.assertEqual(copied.read_bytes(), uninstaller_bytes)
                    self.assertNotIn("installer-smoke-complete", phases)
                cleanup_observations.append(condition)
                shutil.rmtree(scratch)

            proxy = types.SimpleNamespace(name=str(scratch), cleanup=cleanup)
            diagnostics = io.StringIO()
            with ExitStack() as stack:
                stack.enter_context(patch.object(release.tempfile, "TemporaryDirectory", return_value=proxy))
                stack.enter_context(patch.dict(sys.modules, winreg=registry))
                stack.enter_context(patch.object(release, "smoke_environment", return_value={}))
                stack.enter_context(patch.object(release, "capture_preflight", return_value=subprocess.CompletedProcess("checker", 0, b"")))
                stack.enter_context(patch.object(release, "run", side_effect=command))
                stack.enter_context(patch.object(release.subprocess, "run", side_effect=lambda arguments, **_options: subprocess.CompletedProcess(arguments, 1, b"", b"Maple ACP requires sign-in.")))
                stack.enter_context(patch.object(release, "regular", side_effect=require_regular))
                stack.enter_context(patch.object(release, "verify_payload"))
                stack.enter_context(patch.object(release, "signature"))
                stack.enter_context(patch.object(release, "quiet_installer_smoke"))
                stack.enter_context(patch.object(release, "gui_smoke"))
                stack.enter_context(patch.object(release, "WindowsIntegrationProbe", return_value=types.SimpleNamespace(seed=lambda: None, ready=lambda: None)))
                stack.enter_context(patch.object(release, "smoke_application_baseline", return_value=nullcontext((object(), []))))
                stack.enter_context(patch.object(release, "pin_current_pointer", return_value=nullcontext()))
                stack.enter_context(patch.object(release, "interactive_installer_smoke", side_effect=finish_interactive))
                invoked = stack.enter_context(patch.object(release, "run_copied_uninstaller", side_effect=uninstall))
                stack.enter_context(patch.object(release, "smoke_phase_complete", side_effect=lambda phase, *_arguments, **_facts: phases.append(phase)))
                stack.enter_context(patch.object(sys, "stderr", diagnostics))
                if condition == "valid":
                    release.smoke_installer(Path("fixture.exe"), identity, True, "12", expected_hashes,
                                            uninstaller_sha256=uninstaller_sha256, checker=Path("checker.exe"), arch="aarch64")
                    self.assertEqual(invoked.call_count, 1)
                    self.assertIn("installer-smoke-complete", phases)
                else:
                    with self.assertRaises(PermissionError if condition == "denied" else ValueError) as raised:
                        release.smoke_installer(Path("fixture.exe"), identity, True, "12", expected_hashes,
                                                uninstaller_sha256=uninstaller_sha256, checker=Path("checker.exe"), arch="aarch64")
                    if condition == "denied":
                        self.assertIs(raised.exception, denied)
                    if condition == "late-missing":
                        self.assertEqual(invoked.call_count, 1)
                    else:
                        invoked.assert_not_called()
                    self.assertNotIn("uninstall-owned-files-and-state", phases)
                    self.assertNotIn("installer-smoke-complete", phases)
            self.assertEqual(cleanup_observations, [condition])
            release.require_absent(copied, "Auxiliary copy survived temporary cleanup")
            release.require_absent(copied.parent, "Auxiliary directory survived temporary cleanup")
            release.require_absent(scratch, "Scratch survived temporary cleanup")

    def test_valid_success_proves_product_uninstall_before_generic_scratch_cleanup(self):
        self.exercise_success_cleanup("valid")

    def test_success_cannot_skip_a_missing_or_directory_uninstaller(self):
        for condition in ("missing", "directory"):
            with self.subTest(condition=condition):
                self.exercise_success_cleanup(condition)

    def test_success_cannot_translate_installed_uninstaller_access_denial_into_absence(self):
        self.exercise_success_cleanup("denied")

    def test_success_cannot_skip_an_uninstaller_that_disappears_after_admission(self):
        self.exercise_success_cleanup("late-missing")


class VerificationScratchCleanupCallbackTests(unittest.TestCase):
    def test_callback_observes_actual_cleanup_failure_before_cleanup_logs(self):
        with tempfile.TemporaryDirectory() as temporary:
            scratch = (Path(temporary) / "scratch").resolve()
            scratch.mkdir()
            cleanup_error = PermissionError("fixed cleanup failure")
            output, diagnostics, observations = io.StringIO(), io.StringIO(), []

            def cleanup():
                raise cleanup_error

            def callback(owner, error):
                self.assertEqual(owner, scratch)
                self.assertIs(error, cleanup_error)
                self.assertTrue(scratch.is_dir())
                # The failure and timing record must describe the cleanup that
                # already happened, after its optional diagnostic observation.
                self.assertEqual(output.getvalue(), "")
                self.assertEqual(diagnostics.getvalue(), "")
                observations.append((owner, error))

            proxy = types.SimpleNamespace(name=str(scratch), cleanup=cleanup)
            with patch.object(release.tempfile, "TemporaryDirectory", return_value=proxy), \
                    patch.object(sys, "stdout", output), patch.object(sys, "stderr", diagnostics):
                with self.assertRaises(PermissionError) as raised:
                    with release.verification_scratch("fixture-", cleanup_failure=callback):
                        pass
            self.assertIs(raised.exception, cleanup_error)
            self.assertEqual(observations, [(scratch, cleanup_error)])
            self.assertIn("Windows verification cleanup failure: PermissionError", diagnostics.getvalue())
            record = json.loads(output.getvalue().split("Windows verification cleanup: ", 1)[1])
            self.assertEqual(record["result"], "failure")
            self.assertEqual(record["error"], "PermissionError")

    def test_callback_never_runs_when_standard_cleanup_succeeds(self):
        import shutil

        with tempfile.TemporaryDirectory() as temporary:
            scratch = (Path(temporary) / "scratch").resolve()
            scratch.mkdir()
            callback = Mock(side_effect=AssertionError("a successful cleanup has no diagnostic callback"))
            proxy = types.SimpleNamespace(name=str(scratch), cleanup=lambda: shutil.rmtree(scratch))
            with patch.object(release.tempfile, "TemporaryDirectory", return_value=proxy), \
                    patch.object(sys, "stdout", io.StringIO()):
                with release.verification_scratch("fixture-", cleanup_failure=callback):
                    pass
            callback.assert_not_called()
            release.require_scratch_absent(scratch)

    def test_default_callback_is_absent_and_launches_no_diagnostic_worker(self):
        with tempfile.TemporaryDirectory() as temporary:
            scratch = (Path(temporary) / "scratch").resolve()
            scratch.mkdir()
            cleanup_error = PermissionError("fixed default cleanup failure")
            proxy = types.SimpleNamespace(name=str(scratch), cleanup=Mock(side_effect=cleanup_error))
            with patch.object(release.tempfile, "TemporaryDirectory", return_value=proxy), \
                    patch.object(release.subprocess, "run") as worker, \
                    patch.object(sys, "stdout", io.StringIO()), patch.object(sys, "stderr", io.StringIO()):
                with self.assertRaises(PermissionError) as raised:
                    # The default must not enroll ordinary verifier callers in
                    # platform diagnostics or create any native subprocess.
                    with release.verification_scratch("fixture-"):
                        pass
            self.assertIs(raised.exception, cleanup_error)
            proxy.cleanup.assert_called_once_with()
            worker.assert_not_called()

    def test_callback_exception_cannot_replace_cleanup_or_earlier_product_error(self):
        for earlier in (None, ValueError("fixed earlier product failure")):
            with self.subTest(earlier=type(earlier).__name__), tempfile.TemporaryDirectory() as temporary:
                scratch = (Path(temporary) / "scratch").resolve()
                scratch.mkdir()
                cleanup_error = PermissionError("fixed secondary cleanup failure")
                callback_error = RuntimeError("fixed callback failure")
                expected = earlier if earlier is not None else cleanup_error
                callback = Mock(side_effect=callback_error)
                proxy = types.SimpleNamespace(name=str(scratch), cleanup=Mock(side_effect=cleanup_error))
                output, diagnostics = io.StringIO(), io.StringIO()
                with patch.object(release.tempfile, "TemporaryDirectory", return_value=proxy), \
                        patch.object(sys, "stdout", output), patch.object(sys, "stderr", diagnostics):
                    with self.assertRaises(type(expected)) as raised:
                        with release.verification_scratch("fixture-", cleanup_failure=callback):
                            if earlier is not None:
                                raise earlier
                self.assertIs(raised.exception, expected)
                callback.assert_called_once_with(scratch, cleanup_error)
                proxy.cleanup.assert_called_once_with()
                self.assertIn("Windows verification cleanup failure: PermissionError", diagnostics.getvalue())
                record = json.loads(output.getvalue().split("Windows verification cleanup: ", 1)[1])
                self.assertEqual(record["result"], "failure")
                self.assertEqual(record["error"], "PermissionError")


class VerificationScratchLifecycleTests(unittest.TestCase):
    def test_path_resolution_failure_still_cleans_the_created_scratch(self):
        with tempfile.TemporaryDirectory() as temporary:
            real = tempfile.TemporaryDirectory(dir=temporary)
            scratch = Path(real.name).resolve()
            primary = PermissionError("fixed scratch resolution failure")
            proxy = types.SimpleNamespace(name=real.name, cleanup=Mock(side_effect=real.cleanup))
            with patch.object(release.tempfile, "TemporaryDirectory", return_value=proxy), patch.object(Path, "resolve", side_effect=primary), patch.object(sys, "stdout", io.StringIO()):
                with self.assertRaises(PermissionError) as raised:
                    with release.verification_scratch("fixture-"):
                        self.fail("unresolved scratch was yielded")
            self.assertIs(raised.exception, primary)
            proxy.cleanup.assert_called_once_with()
            release.require_scratch_absent(scratch)

    def test_cleanup_runs_after_product_assertions_and_is_strictly_observed(self):
        with tempfile.TemporaryDirectory() as temporary:
            real = tempfile.TemporaryDirectory(dir=temporary)
            scratch = Path(real.name).resolve()
            product_checked = False
            copied = scratch / "uninstaller-runner/uninstall.exe"
            def cleanup():
                self.assertTrue(product_checked)
                self.assertEqual(copied.read_bytes(), b"owned auxiliary runner")
                real.cleanup()
            proxy = types.SimpleNamespace(name=real.name, cleanup=cleanup)
            with patch.object(release.tempfile, "TemporaryDirectory", return_value=proxy) as factory:
                with release.verification_scratch("fixture-") as actual:
                    self.assertEqual(actual, scratch)
                    copied.parent.mkdir()
                    copied.write_bytes(b"owned auxiliary runner")
                    # Product assertions must finish while the auxiliary input
                    # still exists; generic deletion cannot supply their proof.
                    release.require_absent(scratch / "install with spaces/maple-agent.exe", "owned app remained")
                    product_checked = True
                factory.assert_called_once_with(prefix="fixture-", ignore_cleanup_errors=False)
            release.require_scratch_absent(scratch)

    def test_cleanup_failure_is_fatal_and_cannot_replace_a_primary_failure(self):
        for primary in (None, subprocess.CalledProcessError(52, "actual copied child")):
            with self.subTest(primary=primary), tempfile.TemporaryDirectory() as temporary:
                scratch = Path(temporary) / "scratch"
                scratch.mkdir()
                secondary = PermissionError("do not expose arbitrary cleanup details")
                proxy = types.SimpleNamespace(name=str(scratch), cleanup=Mock(side_effect=secondary))
                output, diagnostics = io.StringIO(), io.StringIO()
                with patch.object(release.tempfile, "TemporaryDirectory", return_value=proxy), patch.object(sys, "stdout", output), patch.object(sys, "stderr", diagnostics):
                    with self.assertRaises(type(primary or secondary)) as raised:
                        with release.verification_scratch("fixture-"):
                            if primary:
                                raise primary
                self.assertIs(raised.exception, primary or secondary)
                proxy.cleanup.assert_called_once_with()
                self.assertIn('"result": "failure"', output.getvalue())
                self.assertNotIn("do not expose", output.getvalue() + diagnostics.getvalue())

    def test_cleanup_that_leaves_the_scratch_cannot_report_success(self):
        with tempfile.TemporaryDirectory() as temporary:
            scratch = Path(temporary) / "scratch"
            scratch.mkdir()
            proxy = types.SimpleNamespace(name=str(scratch), cleanup=Mock())
            output = io.StringIO()
            with patch.object(release.tempfile, "TemporaryDirectory", return_value=proxy), patch.object(sys, "stdout", output), patch.object(sys, "stderr", io.StringIO()):
                with self.assertRaisesRegex(ValueError, "scratch survived cleanup"):
                    with release.verification_scratch("fixture-"):
                        pass
            self.assertIn('"result": "failure"', output.getvalue())

    def test_post_cleanup_access_denial_is_never_missing_evidence(self):
        for target in ("copy", "directory", "scratch"):
            with self.subTest(target=target), tempfile.TemporaryDirectory() as temporary:
                real = tempfile.TemporaryDirectory(dir=temporary)
                scratch = Path(real.name).resolve()
                paths = {"copy": scratch / "uninstaller-runner/uninstall.exe", "directory": scratch / "uninstaller-runner", "scratch": scratch}
                after_cleanup = False
                original_lstat = Path.lstat
                denial = PermissionError("fixed post-cleanup lstat denial")
                def probe(path, *arguments, **options):
                    if after_cleanup and path == paths[target]:
                        raise denial
                    return original_lstat(path, *arguments, **options)
                def cleanup():
                    nonlocal after_cleanup
                    real.cleanup()
                    after_cleanup = True
                proxy = types.SimpleNamespace(name=real.name, cleanup=cleanup)
                with patch.object(release.tempfile, "TemporaryDirectory", return_value=proxy), patch.object(Path, "lstat", probe), patch.object(sys, "stdout", io.StringIO()), patch.object(sys, "stderr", io.StringIO()):
                    with self.assertRaises(PermissionError) as raised:
                        with release.verification_scratch("fixture-"):
                            pass
                self.assertIs(raised.exception, denial)


class CopiedUninstallerTests(unittest.TestCase):
    def fixture(self, scratch):
        installed = scratch / "install with spaces"
        installed.mkdir()
        uninstaller = installed / "uninstall.exe"
        uninstaller.write_bytes(b"exact verified NSIS fixture bytes")
        return installed, uninstaller, hashlib.sha256(uninstaller.read_bytes()).hexdigest()

    def test_copy_stays_owned_until_standard_cleanup_after_product_assertions(self):
        for unsigned in (True, False):
            with self.subTest(unsigned=unsigned):
                facts = {}
                with release.verification_scratch("maple-copy-lifecycle-", cleanup_facts=facts) as scratch:
                    installed, uninstaller, digest = self.fixture(scratch)
                    copied = scratch / "uninstaller-runner/uninstall.exe"
                    environment = {"FIXTURE": "isolated"}
                    sentinel = installed / "user-created-file.txt"
                    sentinel.write_bytes(b"preserved user fixture")
                    state = scratch / "data/state-marker.txt"
                    state.parent.mkdir()
                    state.write_bytes(b"preserved state fixture")
                    def uninstall(command, **options):
                        self.assertEqual(command, f'"{copied}" /S _?={installed}')
                        self.assertIs(options["env"], environment)
                        self.assertEqual(options["cwd"], scratch)
                        self.assertEqual(copied.read_bytes(), uninstaller.read_bytes())
                        self.assertEqual(hashlib.sha256(copied.read_bytes()).hexdigest(), digest)
                        uninstaller.unlink()
                        return subprocess.CompletedProcess(command, 0)
                    with patch.object(release, "signature") as signature, patch.object(release, "run", side_effect=uninstall):
                        release.run_copied_uninstaller(uninstaller, installed, scratch, environment, unsigned, digest, "aarch64",
                                                       cleanup_facts=facts)
                    self.assertEqual(signature.call_args_list, [unittest.mock.call(uninstaller, unsigned), unittest.mock.call(copied, unsigned)])
                    release.require_absent(uninstaller, "product uninstaller must already be gone")
                    self.assertEqual(sentinel.read_bytes(), b"preserved user fixture")
                    self.assertEqual(state.read_bytes(), b"preserved state fixture")
                    self.assertEqual(hashlib.sha256(copied.read_bytes()).hexdigest(), digest)
                    self.assertTrue(copied.parent.is_dir())
                    self.assertIs(type(facts["child_returned_monotonic_ns"]), int)
                release.require_absent(copied, "copy must retire only after context cleanup")
                release.require_absent(copied.parent, "runner directory must retire after context cleanup")
                release.require_absent(scratch, "scratch must retire after context cleanup")

    def test_original_remnant_failure_survives_successful_standard_cleanup(self):
        with patch.object(sys, "stdout", io.StringIO()), patch.object(sys, "stderr", io.StringIO()):
            with self.assertRaisesRegex(ValueError, "original installed uninstaller"):
                with release.verification_scratch("maple-product-remnant-") as scratch:
                    installed, uninstaller, digest = self.fixture(scratch)
                    with patch.object(release, "signature"), patch.object(release, "run", return_value=subprocess.CompletedProcess("child", 0)):
                        release.run_copied_uninstaller(uninstaller, installed, scratch, {}, True, digest, "aarch64")
        release.require_scratch_absent(scratch)

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


class NativeCopiedFileOwners:
    """Failure telemetry for one fixture copy; no owner mutation or pass predicate."""
    CAP = 4

    def __init__(self):
        class FileTime(ctypes.Structure):
            _fields_ = [("low", ctypes.c_uint32), ("high", ctypes.c_uint32)]
        class Standard(ctypes.Structure):
            _fields_ = [("allocation", ctypes.c_int64), ("eof", ctypes.c_int64), ("links", ctypes.c_uint32),
                        ("delete_pending", ctypes.c_ubyte), ("directory", ctypes.c_ubyte)]
        class Status(ctypes.Union):
            _fields_ = [("status", ctypes.c_int32), ("pointer", ctypes.c_void_p)]
        class IOStatus(ctypes.Structure):
            _anonymous_ = ("result",)
            _fields_ = [("result", Status), ("information", ctypes.c_size_t)]
        class Pids(ctypes.Structure):
            _fields_ = [("count", ctypes.c_uint32), ("pids", ctypes.c_size_t * self.CAP)]
        class UniqueProcess(ctypes.Structure):
            _fields_ = [("pid", ctypes.c_uint32), ("started", FileTime)]
        class ProcessInfo(ctypes.Structure):
            _fields_ = [("process", UniqueProcess), ("app_name", ctypes.c_uint16 * 256),
                        ("service_name", ctypes.c_uint16 * 64), ("kind", ctypes.c_int32),
                        ("status", ctypes.c_uint32), ("session", ctypes.c_uint32), ("restartable", ctypes.c_int32)]
        self.filetime_type, self.standard_type, self.io_type = FileTime, Standard, IOStatus
        self.pids_type, self.unique_type, self.rm_type = Pids, UniqueProcess, ProcessInfo
        self.kernel = ctypes.WinDLL("kernel32", use_last_error=True)
        self.rm = ctypes.WinDLL("rstrtmgr", use_last_error=True)
        self.nt = ctypes.WinDLL("ntdll", use_last_error=True)
        def bind(library, name, args, result):
            function = getattr(library, name)
            function.argtypes, function.restype = args, result
            return function
        handle, dword, pointer, boolean = ctypes.c_void_p, ctypes.c_uint32, ctypes.c_void_p, ctypes.c_int32
        bind(self.kernel, "CreateFileW", (ctypes.c_wchar_p, dword, dword, pointer, dword, dword, handle), handle)
        bind(self.kernel, "GetFileInformationByHandleEx", (handle, ctypes.c_int32, pointer, dword), boolean)
        bind(self.kernel, "CloseHandle", (handle,), boolean)
        bind(self.kernel, "OpenProcess", (dword, boolean, dword), handle)
        bind(self.kernel, "GetProcessTimes", (handle, pointer, pointer, pointer, pointer), boolean)
        bind(self.kernel, "QueryFullProcessImageNameW", (handle, dword, pointer, pointer), boolean)
        self.query_nt = None
        try:
            self.query_nt = bind(self.nt, "NtQueryInformationFile", (handle, pointer, pointer, dword, ctypes.c_int32), ctypes.c_int32)
        except AttributeError:
            pass
        bind(self.rm, "RmStartSession", (pointer, dword, pointer), dword)
        bind(self.rm, "RmRegisterResources", (dword, dword, pointer, dword, pointer, dword, pointer), dword)
        bind(self.rm, "RmGetList", (dword, pointer, pointer, pointer, pointer), dword)
        bind(self.rm, "RmEndSession", (dword,), dword)

    @staticmethod
    def ticks(value):
        return value.low | (value.high << 32)

    def process(self, pid, expected_creation=None):
        result = {"pid": pid, "diagnostic_process": pid == release.os.getpid()}
        handle = self.kernel.OpenProcess(0x1000, False, pid)  # QUERY_LIMITED_INFORMATION only
        if not handle:
            return {**result, "open_process_winerror": ctypes.get_last_error()}
        try:
            created, exited, kernel, user = (self.filetime_type() for _ in range(4))
            if not self.kernel.GetProcessTimes(handle, ctypes.byref(created), ctypes.byref(exited), ctypes.byref(kernel), ctypes.byref(user)):
                result["process_times_winerror"] = ctypes.get_last_error()
                return result
            result["creation_time"] = self.ticks(created)
            if expected_creation is not None and result["creation_time"] != expected_creation:
                result["pid_reused"] = True
                return result
            name, length = ctypes.create_unicode_buffer(32768), ctypes.c_uint32(32768)
            if self.kernel.QueryFullProcessImageNameW(handle, 0, name, ctypes.byref(length)):
                result["basename"] = release.ntpath.basename(name.value)[:64]
            else:
                result["image_query_winerror"] = ctypes.get_last_error()
            return result
        finally:
            if not self.kernel.CloseHandle(handle):
                result["close_process_winerror"] = ctypes.get_last_error()

    def file(self, path):
        result = {"owner_result": "unknown"}
        # Read only, all share modes, OPEN_EXISTING + OPEN_REPARSE_POINT.
        handle = self.kernel.CreateFileW(str(path), 0x80000000, 7, None, 3, 0x00200000, None)
        if handle == ctypes.c_void_p(-1).value:
            return {**result, "open_file_winerror": ctypes.get_last_error()}
        try:
            standard = self.standard_type()
            if self.kernel.GetFileInformationByHandleEx(handle, 1, ctypes.byref(standard), ctypes.sizeof(standard)):
                result["standard"] = {"links": standard.links, "delete_pending": bool(standard.delete_pending), "directory": bool(standard.directory)}
            else:
                result["standard_winerror"] = ctypes.get_last_error()
            if self.query_nt is None:
                result["nt_query"] = "unavailable"
                return result
            # Class47 is reserved for system use: optional raw telemetry only.
            iosb, pids = self.io_type(), self.pids_type()
            status = self.query_nt(handle, ctypes.byref(iosb), ctypes.byref(pids), ctypes.sizeof(pids), 47)
            result.update(ntstatus=f"0x{status & 0xffffffff:08x}", io_status=f"0x{iosb.status & 0xffffffff:08x}", io_bytes=iosb.information)
            if status == 0 and iosb.status == 0 and iosb.information >= self.pids_type.pids.offset:
                result["pid_count"] = pids.count
                needed = self.pids_type.pids.offset + pids.count * ctypes.sizeof(ctypes.c_size_t)
                if pids.count <= self.CAP and needed <= iosb.information <= ctypes.sizeof(pids):
                    result["owners"] = [self.process(int(pid)) for pid in pids.pids[:pids.count]]
                else:
                    result["truncated_or_short"] = True
            return result
        finally:
            if not self.kernel.CloseHandle(handle):
                result["close_file_winerror"] = ctypes.get_last_error()

    def restart_manager(self, path):
        result = {"owner_result": "unknown"}
        session, key = ctypes.c_uint32(), ctypes.create_unicode_buffer(33)
        result["start_status"] = self.rm.RmStartSession(ctypes.byref(session), 0, key)
        if result["start_status"]:
            return result
        try:
            paths = (ctypes.c_wchar_p * 1)(str(path))
            result["register_status"] = self.rm.RmRegisterResources(session.value, 1, paths, 0, None, 0, None)
            if result["register_status"]:
                return result
            needed, count, reboot = ctypes.c_uint32(), ctypes.c_uint32(self.CAP), ctypes.c_uint32()
            owners = (self.rm_type * self.CAP)()
            result["list_status"] = self.rm.RmGetList(session.value, ctypes.byref(needed), ctypes.byref(count), owners, ctypes.byref(reboot))
            result["needed"], result["count"] = needed.value, count.value
            if result["list_status"] == 0 and count.value <= self.CAP:
                result["owners"] = []
                for owner in owners[:count.value]:
                    creation = self.ticks(owner.process.started)
                    process = self.process(owner.process.pid, creation)
                    process.update(reported_creation_time=creation, kind=owner.kind, status=owner.status,
                                   service=bytes(owner.service_name).decode("utf-16le", "replace").split("\0", 1)[0][:64])
                    result["owners"].append(process)
            elif result["list_status"] == 234 or count.value > self.CAP:
                result["truncated"] = True
            return result
        finally:
            result["end_status"] = self.rm.RmEndSession(session.value)


def native_cleanup_owner_worker():
    collector = NativeCopiedFileOwners()  # Preload before the fixture child runs.
    print("NSIS_OWNER_DIAGNOSTIC_READY", flush=True)
    line = sys.stdin.readline(4097)
    if not line:
        return
    if len(line) > 4096:
        raise ValueError("Owner diagnostic request exceeds its fixed bound")
    request = json.loads(line)
    path = Path(request["path"])
    if not path.is_absolute() or path.name != "uninstall.exe" or path.parent.name != "uninstaller-runner" or not path.parent.parent.name.startswith("maple-nsis-uninstall-fixture-"):
        raise ValueError("Diagnostic path is not this native fixture's exact private copy")
    received = release.time.monotonic_ns()
    for offset in (0, 0.1, 0.5):
        delay = offset - (release.time.monotonic_ns() - received) / 1_000_000_000
        if delay > 0:
            release.time.sleep(delay)  # Observation only; never retry deletion.
        started = release.time.monotonic_ns()
        result = {"inconclusive": True, "callback_monotonic_ns": request["callback_monotonic_ns"],
                  "request_received_monotonic_ns": received, "query_started_monotonic_ns": started,
                  "file": collector.file(path), "restart_manager": collector.restart_manager(path)}
        result["query_finished_monotonic_ns"] = release.time.monotonic_ns()
        print(json.dumps(result, sort_keys=True), flush=True)


class NativeCleanupFailureObserver:
    """Preloaded, read-only failure observer; only its own worker may be stopped."""
    def __enter__(self):
        self.worker, self.unavailable = None, None
        allowed = {"SYSTEMROOT", "WINDIR", "COMSPEC", "TEMP", "TMP"}
        environment = {key: value for key, value in release.os.environ.items() if key.upper() in allowed}
        try:
            self.worker = subprocess.Popen([sys.executable, "-I", str(Path(__file__).resolve()), "--nsis-cleanup-owner-worker"],
                                           stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, env=environment)
            timer = release.threading.Timer(3, self.stop_owned_worker)
            timer.start()
            try:
                if self.worker.stdout.readline(64) != b"NSIS_OWNER_DIAGNOSTIC_READY\n":
                    raise ValueError("Owner diagnostic worker did not preload")
            finally:
                timer.cancel()
        except Exception as error:
            self.unavailable = type(error).__name__
            try:
                self.finish()
            except Exception:
                pass  # Optional observer setup cannot replace the fixture result.
        return self.collect

    def collect(self, scratch, _cleanup_error):
        if self.unavailable:
            print("Windows NSIS cleanup owner diagnostic: " + json.dumps({"unavailable": self.unavailable, "inconclusive": True}), flush=True)
            return
        request = json.dumps({"path": str(scratch / "uninstaller-runner/uninstall.exe"), "callback_monotonic_ns": release.time.monotonic_ns()}).encode() + b"\n"
        if len(request) > 4096:
            raise ValueError("Owner diagnostic request exceeds its fixed bound")
        expired = release.threading.Event()
        def expire():
            expired.set()
            self.stop_owned_worker()
        timer = release.threading.Timer(8, expire)
        timer.start()
        try:
            self.worker.stdin.write(request)
            self.worker.stdin.flush()
            output = self.worker.stdout.read(release.DIAGNOSTIC_LIMIT + 1)
            self.worker.wait(timeout=1)
            if len(output) > release.DIAGNOSTIC_LIMIT:
                raise ValueError("Owner diagnostic output exceeds its fixed bound")
            invalid = False
            fields = {"inconclusive", "callback_monotonic_ns", "request_received_monotonic_ns",
                      "query_started_monotonic_ns", "query_finished_monotonic_ns", "file", "restart_manager"}
            for line in output.decode("utf-8", "replace").splitlines():
                try:
                    record = json.loads(line)
                except json.JSONDecodeError:
                    invalid = True
                    continue
                if not isinstance(record, dict) or set(record) != fields or record.get("inconclusive") is not True:
                    invalid = True
                    continue
                print("Windows NSIS cleanup owner diagnostic: " + json.dumps(record, sort_keys=True), flush=True)
            if invalid:
                print("Windows NSIS cleanup owner diagnostic: " + json.dumps({"unavailable": "invalid-or-partial-worker-output", "inconclusive": True}), flush=True)
        finally:
            timer.cancel()
            print("Windows NSIS cleanup owner diagnostic: " + json.dumps({"worker_exit": self.worker.returncode,
                   "timed_out": expired.is_set(), "inconclusive": True}), flush=True)

    def stop_owned_worker(self):
        try:
            self.worker.kill()  # Only this fixture's diagnostic worker, no owner.
        except OSError:
            # A timer can race the worker's normal exit. Never emit a thread
            # traceback or let this optional observation replace fixture failure.
            pass

    def finish(self):
        if self.worker is None:
            return
        try:
            if self.worker.stdin and not self.worker.stdin.closed:
                self.worker.stdin.close()
        finally:
            try:
                try:
                    self.worker.wait(timeout=1)
                except subprocess.TimeoutExpired:
                    self.stop_owned_worker()
                    self.worker.wait(timeout=1)
            finally:
                self.worker.stdout.close()

    def __exit__(self, error_type, _error, _traceback):
        try:
            self.finish()
        except Exception:
            if error_type is None:
                raise


@unittest.skipUnless(sys.platform == "win32", "requires the native Windows NSIS process model")
class NativeNSISUninstallCopyTests(unittest.TestCase):
    def test_verified_copy_retires_original_and_runner_after_real_child_exit(self):
        # The tools and compiled input stay outside each small real-layout
        # installation scratch. Their cleanup cannot delay retiring its runner.
        arch = release.os.environ.get("MAPLE_WINDOWS_ARCH", "x86_64")
        release.require_native_host(arch)
        with release.verification_scratch("maple-nsis-tools-") as tools:
            compiler = release.install_nsis(tools)
            source = tools / "fixture.nsi"
            installer = tools / "fixture-setup.exe"
            payload = tools / "input.txt"
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
            roots = set()
            for iteration in range(1, 4):
                # This is three independent required proofs. A failure raises
                # immediately; a later iteration cannot replace it with success.
                cleanup_facts = {}
                with NativeCleanupFailureObserver() as observe:
                    with release.verification_scratch("maple-nsis-uninstall-fixture-", cleanup_facts=cleanup_facts,
                                                      cleanup_failure=observe) as scratch:
                        self.assertNotIn(scratch, roots)
                        roots.add(scratch)
                        environment = release.smoke_environment(scratch)
                        installed = scratch / "install with spaces"
                        release.run(release.command_as_windows_args(f'"{installer}" /S /D={installed}'),
                                    env=environment, timeout=120)
                        uninstaller = installed / "uninstall.exe"
                        digest = release.uninstaller_digest(uninstaller)
                        sentinel = installed / "user-created-file.txt"
                        sentinel.write_bytes(b"preserve this user file")
                        release.run_copied_uninstaller(uninstaller, installed, scratch, environment, True, digest, arch,
                                                       cleanup_facts=cleanup_facts)
                        release.require_absent(installed / "owned.txt", "NSIS fixture left its installed payload")
                        release.require_absent(uninstaller, "NSIS fixture left the original uninstaller")
                        self.assertEqual({path.name for path in installed.iterdir()}, {sentinel.name})
                        self.assertEqual(sentinel.read_bytes(), b"preserve this user file")
                        release.smoke_phase_complete("nsis-fixture-product-assertions", arch, iteration=iteration)
                release.require_scratch_absent(scratch)
                release.smoke_phase_complete("nsis-fixture-scratch-retired", arch, iteration=iteration)
        release.require_scratch_absent(tools)
        print("Verified three independent pinned NSIS copied-uninstaller rehearsals only; no Maple runtime qualification", flush=True)

class NativeNSISFixtureControlFlowTests(unittest.TestCase):
    def exercise_fixture(self, fail_first):
        from contextlib import ExitStack, nullcontext

        roots, tools = [], []
        primary = subprocess.CalledProcessError(51, "first independent fixture child")
        def toolchain(root):
            tools.append(root)
            directory = root / "nsis"
            directory.mkdir()
            (directory / "tool-input.txt").write_bytes(b"outer tools must not delay inner cleanup")
            return directory / "makensis.exe"
        def environment(root):
            for name in ("config", "data", "cache", "tmp"):
                (root / name).mkdir()
            return {}
        def command(arguments, **_options):
            if not isinstance(arguments, str):
                return subprocess.CompletedProcess(arguments, 0)
            if " /D=" in arguments:
                installed = Path(arguments.split(" /D=", 1)[1])
                roots.append(installed.parent)
                self.assertNotIn(tools[0], installed.parents)
                self.assertEqual({p.name for p in installed.parent.iterdir()}, {"config", "data", "cache", "tmp"})
                installed.mkdir()
                (installed / "owned.txt").write_bytes(b"owned NSIS fixture payload")
                (installed / "uninstall.exe").write_bytes(b"verified fake NSIS input")
            else:
                installed = Path(arguments.split(" _?=", 1)[1])
                if fail_first:
                    raise primary
                (installed / "owned.txt").unlink()
                (installed / "uninstall.exe").unlink()
            return subprocess.CompletedProcess(arguments, 0)
        # Execute the real three-iteration fixture/lifecycle on every host, with
        # only native process/tool calls replaced. This is control-flow proof.
        case = NativeNSISUninstallCopyTests("test_verified_copy_retires_original_and_runner_after_real_child_exit")
        with ExitStack() as stack:
            stack.enter_context(patch.object(sys.modules[__name__], "NativeCleanupFailureObserver", return_value=nullcontext(lambda *_args: None)))
            stack.enter_context(patch.object(release, "require_native_host"))
            stack.enter_context(patch.object(release, "install_nsis", side_effect=toolchain))
            stack.enter_context(patch.object(release, "smoke_environment", side_effect=environment))
            stack.enter_context(patch.object(release, "signature"))
            stack.enter_context(patch.object(release, "run", side_effect=command))
            stack.enter_context(patch.object(sys, "stdout", io.StringIO()))
            stack.enter_context(patch.object(sys, "stderr", io.StringIO()))
            if fail_first:
                with self.assertRaises(subprocess.CalledProcessError) as raised:
                    case.test_verified_copy_retires_original_and_runner_after_real_child_exit()
                self.assertIs(raised.exception, primary)
            else:
                case.test_verified_copy_retires_original_and_runner_after_real_child_exit()
        self.assertEqual(len(roots), 1 if fail_first else 3)
        self.assertEqual(len(set(roots)), len(roots))
        self.assertEqual(len(tools), 1)
        for root in roots + tools:
            release.require_scratch_absent(root)

    def test_three_fresh_small_scratches_pass_before_outer_tool_cleanup(self):
        self.exercise_fixture(False)

    def test_first_failed_instance_stops_without_later_installations(self):
        self.exercise_fixture(True)


class NativeCopiedFileOwnerAPITests(unittest.TestCase):
    def collector(self):
        kernel = types.SimpleNamespace(**{name: Mock() for name in (
            "CreateFileW", "GetFileInformationByHandleEx", "CloseHandle", "OpenProcess",
            "GetProcessTimes", "QueryFullProcessImageNameW",
        )})
        manager = types.SimpleNamespace(**{name: Mock() for name in (
            "RmStartSession", "RmRegisterResources", "RmGetList", "RmEndSession",
        )})
        native = types.SimpleNamespace(NtQueryInformationFile=Mock())
        libraries = {"kernel32": kernel, "rstrtmgr": manager, "ntdll": native}
        with patch.object(ctypes, "WinDLL", side_effect=lambda name, **_options: libraries[name], create=True):
            collector = NativeCopiedFileOwners()
        kernel.CreateFileW.return_value = 123
        kernel.CloseHandle.return_value = 1
        collector.process = Mock(side_effect=lambda pid, *_arguments: {"pid": pid, "basename": "fixture.exe"})
        return collector, kernel, manager, native

    @staticmethod
    def put_dword(pointer, value):
        ctypes.cast(pointer, ctypes.POINTER(ctypes.c_uint32))[0] = value

    def test_windows_64_bit_structs_match_fixed_sdk_sizes_and_offsets(self):
        collector, kernel, manager, native = self.collector()
        self.assertEqual(ctypes.sizeof(ctypes.c_void_p), 8)
        self.assertEqual(ctypes.sizeof(collector.standard_type), 24)
        self.assertEqual((collector.standard_type.links.offset,
                          collector.standard_type.delete_pending.offset,
                          collector.standard_type.directory.offset), (16, 20, 21))
        self.assertEqual(ctypes.sizeof(collector.io_type), 16)
        self.assertEqual(collector.io_type.information.offset, 8)
        self.assertEqual(collector.pids_type.pids.offset, 8)
        self.assertEqual(ctypes.sizeof(collector.pids_type), 8 + 4 * 8)
        self.assertEqual(ctypes.sizeof(collector.unique_type), 12)
        self.assertEqual(collector.unique_type.started.offset, 4)
        self.assertEqual(ctypes.sizeof(collector.rm_type), 668)
        self.assertEqual([getattr(collector.rm_type, name).offset for name in (
            "process", "app_name", "service_name", "kind", "status", "session", "restartable",
        )], [0, 12, 524, 652, 656, 660, 664])
        # Constructing/binding the collector itself must perform no native work.
        for library in (kernel, manager, native):
            self.assertTrue(all(not function.called for function in vars(library).values()))

    def file_query(self, *, status=0, io_status=0, count=2, byte_count=None):
        collector, kernel, manager, native = self.collector()
        path = Path("owned-private-fixture") / "uninstaller-runner" / "uninstall.exe"
        if byte_count is None:
            byte_count = collector.pids_type.pids.offset + count * ctypes.sizeof(ctypes.c_size_t)

        def standard(handle, kind, output, length):
            self.assertEqual((handle, kind, length), (123, 1, 24))
            value = ctypes.cast(output, ctypes.POINTER(collector.standard_type)).contents
            value.links, value.delete_pending, value.directory = 1, 0, 0
            return 1

        def query(handle, output_status, output_pids, length, kind):
            self.assertEqual((handle, kind, length), (123, 47, 8 + 4 * 8))
            io = ctypes.cast(output_status, ctypes.POINTER(collector.io_type)).contents
            io.status, io.information = io_status, byte_count
            pids = ctypes.cast(output_pids, ctypes.POINTER(collector.pids_type)).contents
            pids.count = count
            for index in range(collector.CAP):
                pids.pids[index] = 100 + index
            return status

        kernel.GetFileInformationByHandleEx.side_effect = standard
        native.NtQueryInformationFile.side_effect = query
        with patch.object(ctypes, "get_last_error", return_value=5, create=True):
            result = collector.file(path)
        kernel.CreateFileW.assert_called_once_with(str(path), 0x80000000, 7, None, 3, 0x00200000, None)
        kernel.GetFileInformationByHandleEx.assert_called_once()
        native.NtQueryInformationFile.assert_called_once()
        kernel.CloseHandle.assert_called_once_with(123)
        # Only the requested file was opened; no process enumeration or process
        # mutation function is even available on these fake native libraries.
        kernel.OpenProcess.assert_not_called()
        kernel.GetProcessTimes.assert_not_called()
        kernel.QueryFullProcessImageNameW.assert_not_called()
        self.assertTrue(all(not function.called for function in vars(manager).values()))
        return collector, result

    def test_file_query_uses_one_read_only_shared_handle_and_fixed_buffers(self):
        collector, result = self.file_query()
        self.assertEqual(result["standard"], {"links": 1, "delete_pending": False, "directory": False})
        self.assertEqual(result["ntstatus"], "0x00000000")
        self.assertEqual(result["io_status"], "0x00000000")
        self.assertEqual(result["io_bytes"], 24)
        self.assertEqual([owner["pid"] for owner in result["owners"]], [100, 101])
        self.assertEqual(collector.process.call_args_list, [unittest.mock.call(100), unittest.mock.call(101)])
        self.assertEqual(result["owner_result"], "unknown")

    def test_pending_failed_short_and_overflowing_nt_responses_never_parse_owners(self):
        cases = (
            {"status": -1073741820},  # STATUS_INFO_LENGTH_MISMATCH
            {"status": 0x103},       # STATUS_PENDING is not completed success
            {"io_status": 0x103},
            {"byte_count": 7},       # incomplete header/alignment prefix
            {"byte_count": 15},      # count=2 needs 24 bytes
            {"count": 5, "byte_count": 40},
            {"count": 1, "byte_count": 41},  # API cannot return beyond our fixed buffer
        )
        for options in cases:
            with self.subTest(options=options):
                collector, result = self.file_query(**options)
                collector.process.assert_not_called()
                self.assertNotIn("owners", result)
                self.assertEqual(result["owner_result"], "unknown")

    def manager_query(self, *, start_status=0, register_status=0, list_status=0, count=1, needed=1):
        collector, kernel, manager, _native = self.collector()
        path = Path("owned-private-fixture") / "uninstaller-runner" / "uninstall.exe"

        def start(session, flags, key):
            self.assertEqual(flags, 0)
            self.assertEqual(len(key), 33)
            self.put_dword(session, 17)
            return start_status

        def register(session, file_count, paths, application_count, applications, service_count, services):
            self.assertEqual((session, file_count, len(paths)), (17, 1, 1))
            self.assertEqual(paths[0], str(path))
            self.assertEqual((application_count, applications, service_count, services), (0, None, 0, None))
            return register_status

        def owners(session, needed_pointer, count_pointer, output, reboot_pointer):
            self.assertEqual(session, 17)
            self.assertEqual(ctypes.cast(count_pointer, ctypes.POINTER(ctypes.c_uint32))[0], 4)
            self.assertEqual(len(output), 4)
            self.put_dword(needed_pointer, needed)
            self.put_dword(count_pointer, count)
            self.put_dword(reboot_pointer, 0)
            output[0].process.pid = 212
            output[0].process.started.low, output[0].process.started.high = 9, 2
            output[0].kind, output[0].status = 3, 1
            for index, character in enumerate("fixture-service"):
                output[0].service_name[index] = ord(character)
            # This friendly application name must not be treated as an image
            # basename or accidentally exposed by the bounded owner projection.
            for index, character in enumerate("private arbitrary friendly application label"):
                output[0].app_name[index] = ord(character)
            return list_status

        manager.RmStartSession.side_effect = start
        manager.RmRegisterResources.side_effect = register
        manager.RmGetList.side_effect = owners
        manager.RmEndSession.return_value = 0
        result = collector.restart_manager(path)
        manager.RmStartSession.assert_called_once()
        if start_status:
            manager.RmRegisterResources.assert_not_called()
            manager.RmGetList.assert_not_called()
            manager.RmEndSession.assert_not_called()
        else:
            manager.RmRegisterResources.assert_called_once()
            manager.RmEndSession.assert_called_once_with(17)
            if register_status:
                manager.RmGetList.assert_not_called()
            else:
                manager.RmGetList.assert_called_once()
        self.assertTrue(all(not function.called for function in vars(kernel).values()))
        return collector, manager, result

    def test_restart_manager_registers_only_one_file_and_projects_bounded_owners(self):
        collector, _manager, result = self.manager_query()
        self.assertEqual(result["owners"], [{
            "pid": 212, "basename": "fixture.exe", "reported_creation_time": (2 << 32) | 9,
            "kind": 3, "status": 1, "service": "fixture-service",
        }])
        collector.process.assert_called_once_with(212, (2 << 32) | 9)
        self.assertEqual(result["end_status"], 0)
        self.assertEqual(result["owner_result"], "unknown")
        self.assertNotIn("private arbitrary", json.dumps(result))

    def test_reported_process_identity_exposes_only_basename_and_checks_creation(self):
        collector, kernel, _manager, _native = self.collector()
        collector.process = types.MethodType(NativeCopiedFileOwners.process, collector)
        kernel.OpenProcess.return_value = 99
        def times(_handle, created, *_arguments):
            value = ctypes.cast(created, ctypes.POINTER(collector.filetime_type)).contents
            value.low, value.high = 9, 2
            return 1
        def image(_handle, flags, output, length):
            self.assertEqual(flags, 0)
            output.value = r"C:\Private arbitrary directory\maple-agent.exe"
            self.put_dword(length, len(output.value))
            return 1
        kernel.GetProcessTimes.side_effect = times
        kernel.QueryFullProcessImageNameW.side_effect = image
        observed = collector.process(212, (2 << 32) | 9)
        self.assertEqual(observed["basename"], "maple-agent.exe")
        self.assertNotIn("Private arbitrary", json.dumps(observed))
        kernel.OpenProcess.assert_called_once_with(0x1000, False, 212)
        kernel.CloseHandle.assert_called_once_with(99)
        kernel.QueryFullProcessImageNameW.reset_mock()
        reused = collector.process(212, 1)
        self.assertTrue(reused["pid_reused"])
        self.assertNotIn("basename", reused)
        kernel.QueryFullProcessImageNameW.assert_not_called()

    def test_process_query_denial_stays_unknown_and_closes_any_opened_handle(self):
        collector, kernel, _manager, _native = self.collector()
        collector.process = types.MethodType(NativeCopiedFileOwners.process, collector)
        with patch.object(ctypes, "get_last_error", return_value=5, create=True):
            kernel.OpenProcess.return_value = None
            self.assertEqual(collector.process(212)["open_process_winerror"], 5)
            kernel.CloseHandle.assert_not_called()
            kernel.OpenProcess.return_value = 99
            kernel.GetProcessTimes.return_value = 0
            self.assertEqual(collector.process(212)["process_times_winerror"], 5)
            kernel.CloseHandle.assert_called_once_with(99)

    def test_restart_manager_more_data_or_overflow_never_resizes_or_parses(self):
        for options in ({"list_status": 234, "count": 1, "needed": 9}, {"count": 5, "needed": 5}):
            with self.subTest(options=options):
                collector, manager, result = self.manager_query(**options)
                self.assertTrue(result["truncated"])
                self.assertNotIn("owners", result)
                collector.process.assert_not_called()
                manager.RmGetList.assert_called_once()
                manager.RmEndSession.assert_called_once_with(17)

    def test_restart_manager_ends_started_session_even_after_register_failure(self):
        collector, manager, result = self.manager_query(register_status=5)
        self.assertEqual(result["register_status"], 5)
        self.assertEqual(result["end_status"], 0)
        collector.process.assert_not_called()
        manager.RmEndSession.assert_called_once_with(17)

    def test_restart_manager_start_failure_has_no_session_to_end(self):
        collector, _manager, result = self.manager_query(start_status=353)
        self.assertEqual(result["start_status"], 353)
        self.assertNotIn("end_status", result)
        collector.process.assert_not_called()

    def test_restart_manager_ends_started_session_when_native_query_raises(self):
        path = Path("owned-private-fixture") / "uninstaller-runner" / "uninstall.exe"
        for failed_call in ("RmRegisterResources", "RmGetList"):
            with self.subTest(failed_call=failed_call):
                collector, _kernel, manager, _native = self.collector()
                primary = OSError("fixed native query failure")
                def start(session, _flags, _key):
                    self.put_dword(session, 17)
                    return 0
                manager.RmStartSession.side_effect = start
                manager.RmRegisterResources.return_value = 0
                manager.RmGetList.return_value = 0
                getattr(manager, failed_call).side_effect = primary
                manager.RmEndSession.return_value = 0
                with self.assertRaises(OSError) as raised:
                    collector.restart_manager(path)
                self.assertIs(raised.exception, primary)
                manager.RmEndSession.assert_called_once_with(17)
                collector.process.assert_not_called()


class NativeCleanupOwnerWorkerTests(unittest.TestCase):
    def test_preloaded_worker_observes_only_requested_file_at_fixed_bounded_offsets(self):
        value = 0
        def monotonic_ns():
            return value
        def sleep(seconds):
            nonlocal value
            self.assertGreaterEqual(seconds, 0)
            value += round(seconds * 1_000_000_000)
        copied = Path.cwd() / "maple-nsis-uninstall-fixture-unit/uninstaller-runner/uninstall.exe"
        request = json.dumps({"path": str(copied), "callback_monotonic_ns": 0}) + "\n"
        collector = types.SimpleNamespace(file=Mock(return_value={"owner_result": "unknown"}),
                                          restart_manager=Mock(return_value={"owner_result": "unknown"}))
        output = io.StringIO()
        with patch.object(sys.modules[__name__], "NativeCopiedFileOwners", return_value=collector) as preload, patch.object(sys, "stdin", io.StringIO(request)), patch.object(sys, "stdout", output), patch.object(release.time, "monotonic_ns", side_effect=monotonic_ns), patch.object(release.time, "sleep", side_effect=sleep), patch.object(subprocess, "run") as command:
            native_cleanup_owner_worker()
        preload.assert_called_once_with()
        self.assertTrue(output.getvalue().startswith("NSIS_OWNER_DIAGNOSTIC_READY\n"))
        snapshots = [json.loads(line) for line in output.getvalue().splitlines()[1:]]
        self.assertEqual([row["query_started_monotonic_ns"] for row in snapshots], [0, 100_000_000, 500_000_000])
        self.assertTrue(all(row["inconclusive"] for row in snapshots))
        self.assertEqual(collector.file.call_args_list, [unittest.mock.call(copied)] * 3)
        self.assertEqual(collector.restart_manager.call_args_list, [unittest.mock.call(copied)] * 3)
        command.assert_not_called()

    def test_successful_fixture_eof_never_queries_any_file_owner(self):
        collector = types.SimpleNamespace(file=Mock(), restart_manager=Mock())
        with patch.object(sys.modules[__name__], "NativeCopiedFileOwners", return_value=collector), patch.object(sys, "stdin", io.StringIO("")), patch.object(sys, "stdout", io.StringIO()):
            native_cleanup_owner_worker()
        collector.file.assert_not_called()
        collector.restart_manager.assert_not_called()

    def test_another_path_is_refused_before_any_owner_query(self):
        collector = types.SimpleNamespace(file=Mock(), restart_manager=Mock())
        request = json.dumps({"path": "/other/uninstaller-runner/uninstall.exe", "callback_monotonic_ns": 0}) + "\n"
        with patch.object(sys.modules[__name__], "NativeCopiedFileOwners", return_value=collector), patch.object(sys, "stdin", io.StringIO(request)), patch.object(sys, "stdout", io.StringIO()), self.assertRaisesRegex(ValueError, "exact private copy"):
            native_cleanup_owner_worker()
        collector.file.assert_not_called()
        collector.restart_manager.assert_not_called()

    def test_observer_preloads_and_only_failure_sends_one_bounded_request(self):
        worker = types.SimpleNamespace(stdin=io.BytesIO(), stdout=io.BytesIO(b'NSIS_OWNER_DIAGNOSTIC_READY\n{"inconclusive":true}\n'),
                                       kill=Mock(), wait=Mock(return_value=0), returncode=0)
        timers = []
        def timer(seconds, callback):
            result = types.SimpleNamespace(start=Mock(), cancel=Mock())
            timers.append((seconds, callback, result))
            return result
        scratch = Path.cwd() / "maple-nsis-uninstall-fixture-unit"
        output = io.StringIO()
        with patch.object(subprocess, "Popen", return_value=worker) as spawn, patch.object(release.threading, "Timer", side_effect=timer), patch.object(sys, "stdout", output):
            with NativeCleanupFailureObserver() as observe:
                self.assertEqual(worker.stdin.getvalue(), b"")
                observe(scratch, PermissionError("original cleanup failure"))
                request = json.loads(worker.stdin.getvalue())
                self.assertEqual(request["path"], str(scratch / "uninstaller-runner/uninstall.exe"))
                self.assertIs(type(request["callback_monotonic_ns"]), int)
        self.assertEqual([item[0] for item in timers], [3, 8])
        self.assertTrue(callable(timers[0][1]))
        self.assertTrue(callable(timers[1][1]))
        worker.kill.assert_not_called()
        self.assertTrue(all(item[2].start.call_count == item[2].cancel.call_count == 1 for item in timers))
        self.assertTrue(worker.stdin.closed and worker.stdout.closed)
        self.assertEqual(spawn.call_args.args[0], [sys.executable, "-I", str(Path(__file__).resolve()), "--nsis-cleanup-owner-worker"])
        self.assertTrue({key.upper() for key in spawn.call_args.kwargs["env"]} <= {"SYSTEMROOT", "WINDIR", "COMSPEC", "TEMP", "TMP"})
        self.assertIn('"inconclusive": true', output.getvalue())
        self.assertNotIn("installer-smoke-complete", output.getvalue())

    def test_observer_output_bound_cannot_become_fixture_success(self):
        worker = types.SimpleNamespace(stdin=io.BytesIO(), stdout=io.BytesIO(b"NSIS_OWNER_DIAGNOSTIC_READY\n" + b"x" * (release.DIAGNOSTIC_LIMIT + 1)),
                                       kill=Mock(), wait=Mock(return_value=0), returncode=0)
        with patch.object(subprocess, "Popen", return_value=worker), patch.object(release.threading, "Timer"), patch.object(sys, "stdout", io.StringIO()):
            with NativeCleanupFailureObserver() as observe, self.assertRaisesRegex(ValueError, "fixed bound"):
                observe(Path.cwd() / "maple-nsis-uninstall-fixture-unit", PermissionError("primary"))

    def test_worker_traceback_or_partial_data_is_not_forwarded(self):
        worker = types.SimpleNamespace(stdin=io.BytesIO(), stdout=io.BytesIO(b"NSIS_OWNER_DIAGNOSTIC_READY\nTraceback: private arbitrary error details\n{partial\n"),
                                       kill=Mock(), wait=Mock(return_value=1), returncode=1)
        output = io.StringIO()
        with patch.object(subprocess, "Popen", return_value=worker), patch.object(release.threading, "Timer"), patch.object(sys, "stdout", output):
            with NativeCleanupFailureObserver() as observe:
                observe(Path.cwd() / "maple-nsis-uninstall-fixture-unit", PermissionError("primary"))
        self.assertIn("invalid-or-partial-worker-output", output.getvalue())
        self.assertNotIn("private arbitrary", output.getvalue())
        self.assertNotIn("Traceback", output.getvalue())
        self.assertIn('"timed_out": false', output.getvalue())

    def test_owned_worker_timeout_is_explicit_and_not_a_fixture_pass(self):
        worker = types.SimpleNamespace(stdin=io.BytesIO(), stdout=io.BytesIO(b"NSIS_OWNER_DIAGNOSTIC_READY\n"),
                                       kill=Mock(), wait=Mock(return_value=-1), returncode=-1)
        def timer(seconds, callback):
            return types.SimpleNamespace(start=Mock(side_effect=callback if seconds == 8 else None), cancel=Mock())
        output = io.StringIO()
        with patch.object(subprocess, "Popen", return_value=worker), patch.object(release.threading, "Timer", side_effect=timer), patch.object(sys, "stdout", output):
            with NativeCleanupFailureObserver() as observe:
                observe(Path.cwd() / "maple-nsis-uninstall-fixture-unit", PermissionError("primary"))
        worker.kill.assert_called_once_with()
        self.assertIn('"timed_out": true', output.getvalue())
        self.assertIn('"inconclusive": true', output.getvalue())
        self.assertNotIn("installer-smoke-complete", output.getvalue())

    def test_timer_race_with_already_exited_worker_is_silent(self):
        observer = NativeCleanupFailureObserver()
        observer.worker = types.SimpleNamespace(kill=Mock(side_effect=ProcessLookupError("already exited")))
        output, diagnostics = io.StringIO(), io.StringIO()
        with patch.object(sys, "stdout", output), patch.object(sys, "stderr", diagnostics):
            observer.stop_owned_worker()
        observer.worker.kill.assert_called_once_with()
        self.assertEqual(output.getvalue() + diagnostics.getvalue(), "")


if __name__ == "__main__":
    if sys.argv[1:] == ["--nsis-cleanup-owner-worker"]:
        native_cleanup_owner_worker()
    else:
        unittest.main()
