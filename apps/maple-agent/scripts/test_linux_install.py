#!/usr/bin/env python3
"""First-install boundaries with synthetic bundles; never launch the Agent."""

import errno
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile
import unittest
from unittest import mock


spec = importlib.util.spec_from_file_location(
    "linux_install", Path(__file__).with_name("linux-install.py")
)
installer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(installer)

SOURCE = "1234567890abcdef1234567890abcdef12345678"
IDENTITIES = {
    "prod": ("maple-agent.app", "cloud.opensecret.maple.agent"),
    "dev": ("maple-agent-dev.app", "cloud.opensecret.maple.agent.dev"),
}


def bundle_fixture(parent, profile="prod"):
    root_name, bundle_id = IDENTITIES[profile]
    root = parent / root_name
    descriptor = {
        "format": "maple-agent-linux-bundle-v1", "product": "maple-agent",
        "profile": profile, "version": "0.0.4", "build": "500", "source_sha": SOURCE,
        "bundle_id": bundle_id, "entrypoint": "AppRun", "executable": "usr/bin/maple-agent",
    }
    files = {
        "bundle.json": json.dumps(descriptor, sort_keys=True).encode(),
        "AppRun": b'#!/bin/sh\nprintf "%s\\n" "$@"\n',
        "install.sh": b"#!/bin/sh\nexit 0\n",
        "usr/bin/maple-agent": b"\x7fELFfixture-agent",
        "usr/lib/ld-linux-x86-64.so.2": b"\x7fELFfixture-loader",
        "usr/lib/libc.so.6": b"\x7fELFfixture-libc",
        "usr/share/maple-agent/install.py": b"# synthetic installer payload\n",
        f"usr/share/icons/hicolor/256x256/apps/{bundle_id}.png": b"fixture-public-icon",
    }
    for name, data in files.items():
        path = root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)
        path.chmod(0o755 if name in ("AppRun", "install.sh") or data.startswith(b"\x7fELF") else 0o644)
    (root / "usr/lib/libc-link.so").symlink_to("libc.so.6")
    for path in (root, *root.rglob("*")):
        if path.is_dir() and not path.is_symlink():
            path.chmod(0o755)
    return root


def portable_rename_absent(source, destination):
    # The production implementation's Linux renameat2 is tested separately.
    # This adapter lets first-install file ownership/rollback run on macOS CI.
    if destination.exists() or destination.is_symlink():
        raise installer.InstallError("installation already exists")
    source.rename(destination)


class LinuxInstallTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.home = self.root / "home with 'quotes' and %"
        self.home.mkdir()
        self.source = bundle_fixture(self.root / "download")
        patch = mock.patch.object(installer, "rename_absent", portable_rename_absent)
        patch.start()
        self.addCleanup(patch.stop)

    def test_first_install_copies_bundle_and_integrates_without_profile_data_changes(self):
        data = self.home / ".local/share/maple-agent/store-v1"
        data.mkdir(parents=True)
        sentinel = data / "untouched-fixture"
        sentinel.write_bytes(b"encrypted-profile-fixture")
        before = installer.inventory(self.source)
        paths = installer.install(self.source, self.home)
        self.assertEqual(installer.inventory(paths["bundle"]), before)
        self.assertEqual(installer.inventory(self.source), before)
        self.assertEqual(sentinel.read_bytes(), b"encrypted-profile-fixture")
        self.assertEqual(paths["bundle"], self.home / ".local/maple-agent.app")
        self.assertFalse(paths["wrapper"].is_symlink())
        result = subprocess.run([str(paths["wrapper"]), "argument with spaces", "--literal"],
                                check=True, capture_output=True, text=True)
        self.assertEqual(result.stdout, "argument with spaces\n--literal\n")
        self.assertEqual(stat.S_IMODE(paths["wrapper"].stat().st_mode), 0o755)
        self.assertEqual(stat.S_IMODE(paths["receipt"].stat().st_mode), 0o600)
        desktop = paths["desktop"].read_text()
        self.assertIn("Name=Maple Work\n", desktop)
        self.assertEqual(paths["desktop"].name, "cloud.opensecret.maple.agent.desktop")
        self.assertIn("Exec=\"", desktop)
        self.assertIn("%%", desktop)
        self.assertIn(" %U\n", desktop)
        receipt = json.loads(paths["receipt"].read_text())
        self.assertEqual(receipt["profile"], "prod")
        for kind in ("wrapper", "desktop", "icon"):
            self.assertEqual(receipt["files"][kind], hashlib.sha256(paths[kind].read_bytes()).hexdigest())

    def test_dev_and_prod_have_distinct_bundle_commands_and_desktop_entries(self):
        prod = installer.install(self.source, self.home)
        dev_source = bundle_fixture(self.root / "dev-download", "dev")
        dev = installer.install(dev_source, self.home)
        self.assertEqual(dev["bundle"].name, "maple-agent-dev.app")
        self.assertEqual(dev["wrapper"].name, "maple-agent-dev")
        self.assertEqual(dev["desktop"].name, "cloud.opensecret.maple.agent.dev.desktop")
        self.assertIn("Name=Maple Work Dev\n", dev["desktop"].read_text())
        self.assertTrue(set(prod.values()).isdisjoint(dev.values()))

    @unittest.skipUnless(sys.platform == "linux" and shutil.which("gio"),
                         "GLib's desktop launcher is required")
    def test_gnome_desktop_entry_launches_installed_bundle_after_download_is_removed(self):
        home = self.root / "desktop home with 'quotes'"
        home.mkdir()
        for profile in ("prod", "dev"):
            with self.subTest(profile=profile):
                source = bundle_fixture(self.root / ("desktop-" + profile), profile)
                paths = installer.install(source, home)
                shutil.rmtree(source)
                # Use the same desktop-entry implementation as GNOME, not a
                # shell approximation of Exec. The fixture AppRun only echoes
                # arguments; no real app, account or desktop session is used.
                result = subprocess.run(
                    ["gio", "launch", str(paths["desktop"]), "https://example.invalid/launch"],
                    capture_output=True, text=True, timeout=10,
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout, "https://example.invalid/launch\n")

    def test_every_preexisting_managed_path_is_refused_without_replacement(self):
        for kind in ("bundle", "wrapper", "desktop", "icon", "receipt"):
            with self.subTest(kind=kind):
                home = self.root / ("existing-" + kind)
                home.mkdir()
                paths = installer.layout(home, "prod")
                occupied = paths[kind]
                occupied.parent.mkdir(parents=True, exist_ok=True)
                occupied.write_bytes(b"existing-owner-content")
                with self.assertRaisesRegex(installer.InstallError, "already exists"):
                    installer.install(self.source, home)
                self.assertEqual(occupied.read_bytes(), b"existing-owner-content")
                self.assertFalse(any(path.exists() for name, path in paths.items() if name != kind))

    def test_symlink_managed_parent_and_destination_are_refused(self):
        outside = self.root / "outside"
        outside.mkdir()
        (self.home / ".local").symlink_to(outside, target_is_directory=True)
        with self.assertRaisesRegex(installer.InstallError, "owned directory"):
            installer.install(self.source, self.home)
        self.assertEqual(list(outside.iterdir()), [])
        (self.home / ".local").unlink()
        (self.home / ".local").mkdir()
        destination = self.home / ".local/maple-agent.app"
        destination.symlink_to(outside, target_is_directory=True)
        with self.assertRaisesRegex(installer.InstallError, "already exists"):
            installer.install(self.source, self.home)
        self.assertTrue(destination.is_symlink())

    def test_integration_failure_rolls_back_only_new_install_and_keeps_profile_data(self):
        sentinel = self.home / "profile-preservation-fixture"
        sentinel.write_bytes(b"keep")
        original = installer.exclusive_write
        calls = 0

        def fail_second_output(path, data, mode):
            nonlocal calls
            calls += 1
            if calls == 2:
                raise OSError("fixture disk failure")
            original(path, data, mode)

        with mock.patch.object(installer, "exclusive_write", side_effect=fail_second_output):
            with self.assertRaisesRegex(OSError, "fixture disk failure"):
                installer.install(self.source, self.home)
        self.assertFalse(any(path.exists() for path in installer.layout(self.home, "prod").values()))
        self.assertEqual(sentinel.read_bytes(), b"keep")
        self.assertTrue(self.source.is_dir())

    def test_source_mutation_during_copy_is_refused_before_publication(self):
        original = shutil.copytree

        def changed_copy(source, destination, *args, **kwargs):
            result = original(source, destination, *args, **kwargs)
            if Path(source) == self.source:
                (Path(destination) / "usr/share/maple-agent/install.py").write_bytes(b"changed")
            return result

        with mock.patch.object(installer.shutil, "copytree", side_effect=changed_copy):
            with self.assertRaisesRegex(installer.InstallError, "changed while copying"):
                installer.install(self.source, self.home)
        self.assertFalse(installer.layout(self.home, "prod")["bundle"].exists())

    def test_unsafe_tree_modes_hardlinks_and_escape_are_refused(self):
        cases = {
            "writable": lambda root: (root / "AppRun").chmod(0o777),
            "setid": lambda root: (root / "AppRun").chmod(0o4755),
            "hardlink": lambda root: os.link(root / "AppRun", root / "extra-link"),
            "escape": lambda root: (root / "escape").symlink_to("../../outside"),
            "cycle": lambda root: (root / "cycle").symlink_to("cycle"),
        }
        for label, mutate in cases.items():
            with self.subTest(label=label):
                source = bundle_fixture(self.root / label)
                try:
                    mutate(source)
                except OSError as error:
                    if label == "setid" and error.errno == errno.EPERM:
                        self.skipTest("sandbox refuses setuid fixture creation")
                    raise
                if label == "setid" and not source.joinpath("AppRun").stat().st_mode & stat.S_ISUID:
                    self.skipTest("filesystem strips the setuid bit from fixture files")
                with self.assertRaises(installer.InstallError):
                    installer.validate_bundle(source)

    def test_descriptor_rejects_identity_substitution_and_duplicate_json_keys(self):
        value = json.loads((self.source / "bundle.json").read_text())
        for key, replacement in (("build", "0500"), ("source_sha", "a" * 39),
                                 ("profile", "dev"), ("entrypoint", "../AppRun"),
                                 ("version", "latest")):
            with self.subTest(key=key):
                changed = dict(value, **{key: replacement})
                with self.assertRaises(installer.InstallError):
                    installer.validate_descriptor(changed)
        (self.source / "bundle.json").write_text('{"profile":"prod","profile":"dev"}')
        with self.assertRaisesRegex(installer.InstallError, "duplicate"):
            installer.read_json(self.source / "bundle.json")

    @unittest.skipUnless(shutil.which("tar"), "tar is required for the documented first-install path")
    def test_documented_permission_preserving_extraction_under_umask_077(self):
        archive = self.root / "download.tar.gz"
        with tarfile.open(archive, "w:gz", format=tarfile.GNU_FORMAT, dereference=False) as target:
            target.add(self.source, arcname=self.source.name)
        extracted = self.root / "extracted"
        extracted.mkdir()
        subprocess.run(["/bin/sh", "-c", 'umask 077; exec tar -xzpf "$1" -C "$2"',
                        "documented-extraction", str(archive), str(extracted)], check=True)
        source = extracted / "maple-agent.app"
        self.assertEqual(stat.S_IMODE(source.stat().st_mode), 0o755)
        self.assertEqual(stat.S_IMODE((source / "bundle.json").stat().st_mode), 0o644)
        paths = installer.install(source, self.home)
        self.assertTrue(paths["wrapper"].is_file())
        self.assertEqual(stat.S_IMODE((paths["bundle"] / "AppRun").stat().st_mode), 0o755)

    @unittest.skipUnless(sys.platform == "linux", "renameat2 is Linux-specific")
    def test_real_atomic_no_replace_preserves_concurrent_destination(self):
        source = self.root / "rename-source"
        destination = self.root / "rename-destination"
        source.mkdir()
        destination.mkdir()
        sentinel = destination / "owner"
        sentinel.write_bytes(b"concurrent installation")
        # Bypass only this fixture's portability adapter.
        with mock.patch.object(installer, "rename_absent", self.real_rename):
            with self.assertRaisesRegex(installer.InstallError, "already exists"):
                installer.rename_absent(source, destination)
        self.assertTrue(source.is_dir())
        self.assertEqual(sentinel.read_bytes(), b"concurrent installation")

    real_rename = staticmethod(installer.rename_absent)


if __name__ == "__main__":
    unittest.main()
