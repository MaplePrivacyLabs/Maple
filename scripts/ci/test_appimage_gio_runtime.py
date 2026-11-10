#!/usr/bin/env python3
"""The AppImage carries the GIO TLS module and GTK schemas its libraries load at runtime."""

import os
from pathlib import Path
import re
import shutil
import stat
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
PROBE = ROOT / "scripts/ci/appimage_gio_runtime.py"


class AppImageGioRuntimeTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)
        self.appdir = self.root / "AppDir"
        self.store = self.root / "store"
        (self.appdir / "usr/lib").mkdir(parents=True)

    def add_webkit(self):
        (self.appdir / "usr/lib/libwebkit2gtk-4.1.so.0").write_bytes(b"")

    def add_store_file(self, relative, content=b"payload"):
        path = self.store / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(content)
        # Nix store files are read-only, and later packaging steps rewrite the copies.
        path.chmod(0o444)

    def run_shell(self, commands, **extra_env):
        closure = self.root / "closure"
        closure.mkdir(exist_ok=True)
        roots = sorted(path for path in self.store.iterdir()) if self.store.is_dir() else []
        (closure / "store-paths").write_text("".join(f"{path}\n" for path in roots))
        env = dict(os.environ, APPDIR_FIXTURE=str(self.appdir), SOURCE_DATE_EPOCH="1234567890",
                   MAPLE_NIX_LINUX_CLOSURE_INFO=str(closure), **extra_env)
        return subprocess.run(["bash", "-c", "source scripts/ci/_common.sh\n" + commands],
                              cwd=ROOT, env=env, capture_output=True, text=True)

    def stage(self):
        return self.run_shell('stage_linux_appdir_gio_runtime "$APPDIR_FIXTURE"')

    def add_runtime_files(self):
        self.add_store_file("glib-networking/lib/gio/modules/libgiognutls.so", b"tls module")
        self.add_store_file("gtk3/lib/libgtk-3.so.0")
        self.add_store_file("gtk3/share/gsettings-schemas/gtk+3-3.24/glib-2.0/schemas/gschemas.compiled", b"schemas")

    def test_the_tls_module_and_gtk_schemas_are_staged_writable(self):
        self.add_webkit()
        self.add_runtime_files()
        result = self.stage()
        self.assertEqual(result.returncode, 0, result.stderr)
        module = self.appdir / "usr/lib/gio/modules/libgiognutls.so"
        schemas = self.appdir / "usr/share/glib-2.0/schemas/gschemas.compiled"
        self.assertEqual(module.read_bytes(), b"tls module")
        self.assertEqual(schemas.read_bytes(), b"schemas")
        for path in (module, schemas):
            self.assertTrue(path.stat().st_mode & stat.S_IWUSR, path)

    def test_only_gtk3_supplies_the_schemas(self):
        self.add_webkit()
        self.add_runtime_files()
        # Sorts before gtk3 in the closure, and is not GTK3.
        self.add_store_file("aaa/share/gsettings-schemas/other/glib-2.0/schemas/gschemas.compiled", b"other")
        result = self.stage()
        self.assertEqual(result.returncode, 0, result.stderr)
        schemas = self.appdir / "usr/share/glib-2.0/schemas/gschemas.compiled"
        self.assertEqual(schemas.read_bytes(), b"schemas")

    def test_a_closure_missing_either_file_fails_the_build(self):
        self.add_webkit()
        self.add_store_file("gtk3/lib/libgtk-3.so.0")
        self.add_store_file("gtk3/share/gsettings-schemas/gtk+3-3.24/glib-2.0/schemas/gschemas.compiled")
        result = self.stage()
        self.assertEqual(result.returncode, 1)
        self.assertIn("no GIO TLS module", result.stderr)

        self.add_store_file("glib-networking/lib/gio/modules/libgiognutls.so")
        (self.store / "gtk3/share/gsettings-schemas/gtk+3-3.24/glib-2.0/schemas/gschemas.compiled").unlink()
        result = self.stage()
        self.assertEqual(result.returncode, 1)
        self.assertIn("no compiled GTK3 GSettings schemas", result.stderr)

    def test_an_appdir_without_webkit_is_left_alone(self):
        result = self.run_shell('stage_linux_appdir_gio_runtime "$APPDIR_FIXTURE"\n'
                                'verify_linux_appdir_gio_runtime "$APPDIR_FIXTURE"')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse((self.appdir / "usr/lib/gio").exists())

    def test_apprun_points_gio_at_the_bundled_modules(self):
        result = self.run_shell('write_linux_appimage_maple_apprun "$APPDIR_FIXTURE"')
        self.assertEqual(result.returncode, 0, result.stderr)
        app_run = self.appdir / "AppRun"
        hooks = self.appdir / "apprun-hooks"
        hooks.mkdir()
        for name in re.findall(r'apprun-hooks/"([^"]+)"', app_run.read_text()):
            (hooks / name).write_text("")
        maple = self.appdir / "usr/bin/maple"
        maple.parent.mkdir(parents=True)
        maple.write_text(f"#!{shutil.which('bash')}\nprintf '%s\\n' \"$GIO_MODULE_DIR\"\n")
        maple.chmod(0o755)
        # Set the way a host or a dev shell would; the bundled module must still win.
        env = dict(os.environ, GIO_MODULE_DIR="/usr/lib/gio/modules")
        env.pop("APPDIR", None)
        result = subprocess.run([shutil.which("bash"), str(app_run)], env=env, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), f"{self.appdir.resolve()}/usr/lib/gio/modules")

    def test_verification_fails_when_the_bundled_libraries_cannot_load(self):
        self.add_webkit()
        result = self.run_shell('verify_linux_appdir_gio_runtime "$APPDIR_FIXTURE"')
        self.assertEqual(result.returncode, 1)
        self.assertIn("AppImage runtime check failed", result.stderr)

    def test_the_probe_rejects_a_library_from_outside_the_appdir(self):
        # Whatever the host provides is outside this empty AppDir, or absent.
        result = subprocess.run([sys.executable, "-I", str(PROBE), str(self.appdir)],
                                capture_output=True, text=True)
        self.assertEqual(result.returncode, 1)
        self.assertIn("AppImage runtime check failed", result.stderr)
        self.assertNotIn("verified-linux-appimage-gio-runtime", result.stdout)


if __name__ == "__main__":
    unittest.main()
