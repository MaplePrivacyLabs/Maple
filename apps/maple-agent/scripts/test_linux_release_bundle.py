#!/usr/bin/env python3
"""Synthetic, data-only Linux bundle archive boundary fixtures."""

import gzip
import importlib.util
import io
import json
import os
from pathlib import Path
import stat
import subprocess
import sys
import tarfile
import tempfile
import unittest
from unittest import mock


spec = importlib.util.spec_from_file_location("linux_release_bundle", Path(__file__).with_name("linux-release-bundle.py"))
bundle = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bundle)

ROOT = "maple-agent.app"
DESCRIPTOR = {
    "format": "maple-agent-linux-bundle-v1", "product": "maple-agent", "profile": "prod",
    "version": "0.0.4", "build": "42", "source_sha": "a" * 40,
    "bundle_id": "cloud.opensecret.maple.agent", "entrypoint": "AppRun",
    "executable": "usr/bin/maple-agent",
}


def entry(name, data=b"fixture data\n", mode=0o644, kind=tarfile.REGTYPE, target=""):
    return name, data, mode, kind, target


def fixture_entries(descriptor=None):
    """Explicit public layout; fake ELF bytes are never executable programs."""
    directories = (ROOT, ROOT + "/usr", ROOT + "/usr/bin", ROOT + "/usr/lib",
                   ROOT + "/usr/share", ROOT + "/usr/share/maple-agent")
    return [*(entry(name, b"", 0o755, tarfile.DIRTYPE) for name in directories),
            entry(ROOT + "/bundle.json", json.dumps(DESCRIPTOR if descriptor is None else descriptor).encode()),
            entry(ROOT + "/AppRun", b"#!/bin/sh\nexit 71\n", 0o755),
            entry(ROOT + "/install.sh", b"#!/bin/sh\nexit 72\n", 0o755),
            entry(ROOT + "/usr/bin/maple-agent", b"\x7fELFsynthetic-agent", 0o755),
            # Bundle format v1 compatibility launcher: an executable shell script.
            entry(ROOT + "/usr/lib/ld-linux-x86-64.so.2", b"#!/bin/sh\nexit 74\n", 0o755),
            entry(ROOT + "/usr/lib/libc.so.6", b"\x7fELFsynthetic-libc", 0o755),
            entry(ROOT + "/usr/share/maple-agent/install.py", b"raise SystemExit(73)\n"),
            entry(ROOT + "/usr/lib/libc-alias.so", b"", 0o777, tarfile.SYMTYPE, "libc.so.6")]


class LinuxBundleArchiveTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.directory = Path(self.temporary.name)
        self.archive = self.directory / "fixture.tar.gz"

    def write_archive(self, entries=None, archive_format=tarfile.GNU_FORMAT):
        entries = fixture_entries() if entries is None else entries
        with tarfile.open(self.archive, "w:gz", format=archive_format) as target:
            for name, data, mode, kind, link in entries:
                member = tarfile.TarInfo(name)
                member.type, member.mode, member.linkname = kind, mode, link
                member.size = len(data) if kind in (tarfile.REGTYPE, tarfile.GNUTYPE_LONGNAME,
                                                   tarfile.GNUTYPE_LONGLINK, tarfile.XHDTYPE, tarfile.XGLTYPE) else 0
                target.addfile(member, io.BytesIO(data) if member.size else None)
        return self.archive

    def reject(self, entries, message=None):
        self.write_archive(entries)
        if message is None:
            with self.assertRaises(bundle.install.InstallError):
                bundle.inspect_archive(self.archive, "prod")
        else:
            with self.assertRaisesRegex(bundle.install.InstallError, message):
                bundle.inspect_archive(self.archive, "prod")

    def test_valid_archive_extracts_exact_payload_and_modes_without_execution(self):
        self.write_archive()
        output = self.directory / "private-extraction"
        previous = os.umask(0o077)
        try:
            with mock.patch.object(bundle.stage, "run", side_effect=AssertionError("artifact execution")):
                descriptor, members = bundle.inspect_archive(self.archive, "prod")
                root = bundle.extract_archive(self.archive, output, "prod")
        finally:
            os.umask(previous)
        self.assertEqual(descriptor, DESCRIPTOR)
        self.assertEqual(root, output / ROOT)
        self.assertEqual(len(members), 14)
        self.assertEqual(stat.S_IMODE(output.stat().st_mode), 0o700)
        self.assertEqual(stat.S_IMODE(root.stat().st_mode), 0o755)
        self.assertEqual((root / "usr/bin/maple-agent").read_bytes(), b"\x7fELFsynthetic-agent")
        self.assertEqual(stat.S_IMODE((root / "AppRun").stat().st_mode), 0o755)
        self.assertEqual(stat.S_IMODE((root / "bundle.json").stat().st_mode), 0o644)
        self.assertEqual((root / "usr/lib/libc-alias.so").readlink(), Path("libc.so.6"))
        self.assertEqual((root / "usr/lib/libc-alias.so").read_bytes(), b"\x7fELFsynthetic-libc")

    def test_extraction_refuses_existing_destination_without_changes(self):
        self.write_archive()
        output = self.directory / "existing"
        output.mkdir()
        marker = output / "keep"
        marker.write_bytes(b"unchanged")
        with self.assertRaisesRegex(bundle.install.InstallError, "must be absent"):
            bundle.extract_archive(self.archive, output, "prod")
        self.assertEqual(marker.read_bytes(), b"unchanged")
        self.assertEqual(list(output.iterdir()), [marker])

    def test_accepts_dev_identity_only_under_its_distinct_root(self):
        descriptor = {**DESCRIPTOR, "profile": "dev", "bundle_id": "cloud.opensecret.maple.agent.dev"}
        entries = [(name.replace(ROOT, "maple-agent-dev.app", 1), data, mode, kind, link)
                   for name, data, mode, kind, link in fixture_entries(descriptor)]
        self.write_archive(entries)
        actual, members = bundle.inspect_archive(self.archive, "dev")
        self.assertEqual(actual, descriptor)
        self.assertIn("maple-agent-dev.app/AppRun", members)
        with self.assertRaisesRegex(bundle.install.InstallError, "profile"):
            bundle.inspect_archive(self.archive, "prod")

    def test_rejects_escaping_and_noncanonical_member_names(self):
        for name in ("/absolute", "../outside", ROOT + "/../outside", ROOT + "//double",
                     ROOT + "/./dot", ROOT + "/back\\slash", ROOT + "/line\nfeed"):
            with self.subTest(name=name):
                self.reject(fixture_entries() + [entry(name)])

    def test_rejects_duplicate_multiple_or_wrong_roots_and_wrong_profile(self):
        self.reject(fixture_entries() + [entry(ROOT + "/bundle.json", b"{}")], "duplicate")
        self.reject(fixture_entries() + [entry("other.app", b"", 0o755, tarfile.DIRTYPE)], "roots")
        renamed = [(name.replace(ROOT, "other.app", 1), data, mode, kind, link)
                   for name, data, mode, kind, link in fixture_entries()]
        self.reject(renamed, "roots")
        self.write_archive()
        with self.assertRaisesRegex(bundle.install.InstallError, "profile"):
            bundle.inspect_archive(self.archive, "dev")

    def test_rejects_missing_payload_and_invalid_descriptor_identity(self):
        self.reject([item for item in fixture_entries() if item[0] != ROOT + "/usr/bin/maple-agent"], "required")
        self.reject([item for item in fixture_entries() if item[0] != ROOT + "/usr/bin/maple-agent"] + [
            entry(ROOT + "/usr/bin/maple-agent", b"", 0o777, tarfile.SYMTYPE, "../lib/libc.so.6")], "required")
        for field, value in (("product", "research"), ("format", "unknown"),
                             ("bundle_id", "cloud.opensecret.maple.agent.dev"),
                             ("entrypoint", "other"), ("executable", "../outside"),
                             ("build", "0"), ("source_sha", "a" * 8)):
            with self.subTest(field=field):
                self.reject(fixture_entries({**DESCRIPTOR, field: value}))
        self.reject(fixture_entries({**DESCRIPTOR, "extra": "not public"}), "fields")
        duplicate = (json.dumps(DESCRIPTOR)[:-1] + ', "product": "maple-agent"}').encode()
        self.reject([(name, duplicate if name == ROOT + "/bundle.json" else data, mode, kind, link)
                     for name, data, mode, kind, link in fixture_entries()], "duplicate")

    def test_rejects_escaping_dangling_cyclic_and_parent_symlinks(self):
        for target in ("/outside", "../../../outside", "missing.so"):
            with self.subTest(target=target):
                self.reject(fixture_entries() + [entry(ROOT + "/usr/lib/unsafe", b"", 0o777,
                                                       tarfile.SYMTYPE, target)])
        self.reject(fixture_entries() + [
            entry(ROOT + "/usr/lib/first", b"", 0o777, tarfile.SYMTYPE, "second"),
            entry(ROOT + "/usr/lib/second", b"", 0o777, tarfile.SYMTYPE, "first")], "cycle")
        self.reject(fixture_entries() + [
            entry(ROOT + "/usr/alias", b"", 0o777, tarfile.SYMTYPE, "lib"),
            entry(ROOT + "/usr/alias/child")], "through a symlink")

    def test_rejects_hardlinks_and_special_files(self):
        for kind in (tarfile.LNKTYPE, tarfile.FIFOTYPE, tarfile.CHRTYPE, tarfile.BLKTYPE):
            with self.subTest(kind=kind):
                self.reject(fixture_entries() + [entry(ROOT + "/special", b"", 0o644, kind,
                                                       ROOT + "/bundle.json")], "unsupported")

    def test_rejects_unsafe_stored_modes(self):
        for name, mode in ((ROOT, 0o700), (ROOT, 0o775), (ROOT + "/bundle.json", 0o666),
                           (ROOT + "/AppRun", 0o750), (ROOT + "/usr/bin/maple-agent", 0o4755)):
            with self.subTest(name=name, mode=oct(mode)):
                entries = [(path, data, mode if path == name else stored, kind, link)
                           for path, data, stored, kind, link in fixture_entries()]
                self.reject(entries, "permissions")

    def test_rejects_oversized_descriptor_before_json_parse(self):
        entries = [(name, b" " * (16 * 1024 + 1) if name == ROOT + "/bundle.json" else data,
                    mode, kind, link) for name, data, mode, kind, link in fixture_entries()]
        self.reject(entries, "oversized bundle descriptor")

    def test_bounds_raw_gnu_and_pax_extension_headers(self):
        for kind in (tarfile.GNUTYPE_LONGNAME, tarfile.GNUTYPE_LONGLINK, tarfile.XHDTYPE, tarfile.XGLTYPE):
            with self.subTest(kind=kind):
                oversized = entry("././@LongLink", b"x" * (16 * 1024 + 1), 0o644, kind)
                self.reject([oversized] + fixture_entries(), "oversized tar extension")

    def test_accepts_bounded_long_names_from_gnu_and_pax_archives(self):
        long_path = ROOT + "/usr/share/" + "long-" * 30 + "notice.txt"
        for archive_format in (tarfile.GNU_FORMAT, tarfile.PAX_FORMAT):
            with self.subTest(archive_format=archive_format):
                self.write_archive(fixture_entries() + [entry(long_path, b"long notice\n")], archive_format)
                descriptor, members = bundle.inspect_archive(self.archive, "prod")
                self.assertEqual(descriptor["profile"], "prod")
                self.assertIn(long_path, members)

    def test_rejects_content_after_tar_end_markers(self):
        self.write_archive()
        raw = gzip.decompress(self.archive.read_bytes())
        self.archive.write_bytes(gzip.compress(raw + b"untrusted trailing content"))
        with self.assertRaisesRegex(bundle.install.InstallError, "trailing tar content"):
            bundle.inspect_archive(self.archive, "prod")


class LinuxBundleLauncherTests(unittest.TestCase):
    def test_built_archives_ship_readable_install_instructions_for_both_profiles(self):
        for profile, root_name in (("prod", ROOT), ("dev", "maple-agent-dev.app")):
            with self.subTest(profile=profile), tempfile.TemporaryDirectory() as temporary:
                temporary = Path(temporary)
                output = temporary / f"maple-agent-{profile}-0.0.4-aaaaaaaa-linux-x86_64.tar.gz"
                metadata = {"channel": profile, "version": "0.0.4", "build_number": "42",
                            "bundle_id": bundle.install.PROFILES[profile][2]}

                def stage_fixture(*args):
                    root = Path(args[-1])
                    root.mkdir()
                    for name, data, mode, kind, link in fixture_entries():
                        relative = Path(name).relative_to(ROOT)
                        path = root / relative
                        if kind == tarfile.DIRTYPE:
                            path.mkdir(exist_ok=True)
                        elif kind == tarfile.SYMTYPE:
                            path.symlink_to(link)
                        else:
                            path.write_bytes(data)
                            path.chmod(mode)

                with mock.patch.object(bundle.stage, "metadata_from_environment", return_value=metadata), \
                     mock.patch.object(bundle.stage, "run", side_effect=stage_fixture), \
                     mock.patch.object(bundle, "audit"), \
                     mock.patch.dict(os.environ, MAPLE_PACKAGE_SOURCE_SHA="a" * 40, SOURCE_DATE_EPOCH="1"):
                    bundle.build(temporary / "unused-binary", output)
                root = bundle.extract_archive(output, temporary / "extracted", profile)
                self.assertEqual(root.name, root_name)
                readme = root / "README.txt"
                self.assertEqual(readme.read_bytes(), (bundle.SCRIPTS / "linux-bundle-README.txt").read_bytes())
                self.assertEqual(stat.S_IMODE(readme.stat().st_mode), 0o644)
                self.assertEqual((root / "install.sh").read_bytes(), (bundle.SCRIPTS / "linux-install.sh").read_bytes())

    @unittest.skipUnless(Path("/lib64/ld-linux-x86-64.so.2").exists(), "needs a glibc host loader")
    def test_clears_same_appdir_image_state_and_preserves_host_environment(self):
        with tempfile.TemporaryDirectory() as temporary:
            app = Path(temporary).resolve() / "maple-agent.app"
            (app / "usr/bin").mkdir(parents=True)
            payload = app / "usr/bin/maple-agent"
            payload.write_text(f"#!{sys.executable}\nimport json, os, sys\n"
                               "print(json.dumps({'env': dict(os.environ), 'argv': sys.argv[1:]}))\n")
            payload.chmod(0o755)
            launcher = app / "AppRun"
            launcher.write_text(bundle.bundle_apprun())
            launcher.chmod(0o755)
            # Bundled client libraries use host data. AppRun only discovers
            # the distro's ALSA plugins; every explicit override is preserved.
            custom = {"FONTCONFIG_FILE": "/custom/fonts.conf", "ALSA_CONFIG_PATH": "/custom/alsa.conf",
                      "ALSA_CONFIG_DIR": "/custom/alsa", "ALSA_PLUGIN_DIR": "/custom/plugins",
                      "PULSE_SERVER": "unix:/custom/pulse", "VK_DRIVER_FILES": "/custom/vulkan.json",
                      "LD_LIBRARY_PATH": "/custom/lib", "XLOCALEDIR": ""}
            empty_audio = {key: "" for key in ("ALSA_CONFIG_PATH", "ALSA_CONFIG_DIR", "ALSA_PLUGIN_DIR")}
            host_plugins = next((str(path) for path in (
                Path("/usr/lib/x86_64-linux-gnu/alsa-lib"), Path("/usr/lib64/alsa-lib"),
                Path("/usr/lib/alsa-lib"),
            ) if path.is_dir()), None)
            for inherited in (custom, empty_audio, {}):
                with self.subTest(inherited=sorted(inherited)):
                    # Keep the pinned shell's utilities available inside Nix sandboxes.
                    environment = {"PATH": os.environ["PATH"], "APPDIR": str(app),
                                   "APPIMAGE": "/stale/outer.AppImage", "APPIMAGE_EXTRACT_AND_RUN": "1", **inherited}
                    result = subprocess.run([str(launcher), "--build-info"], env=environment,
                                            capture_output=True, text=True, timeout=10)
                    self.assertEqual(result.returncode, 0, result.stderr)
                    observed = json.loads(result.stdout)
                    expected = {key: value for key, value in environment.items() if not key.startswith("APPIMAGE")}
                    self.assertEqual({key: observed["env"].get(key) for key in expected}, expected)
                    self.assertNotIn("APPIMAGE", observed["env"])
                    self.assertNotIn("APPIMAGE_EXTRACT_AND_RUN", observed["env"])
                    for key in custom:
                        if key not in inherited:
                            if key == "ALSA_PLUGIN_DIR" and host_plugins is not None:
                                self.assertEqual(observed["env"][key], host_plugins)
                            else:
                                self.assertNotIn(key, observed["env"])
                    self.assertEqual(observed["argv"], ["--build-info"])


if __name__ == "__main__":
    unittest.main()
