#!/usr/bin/env python3
"""The AppImage leaves GBM and libdrm to the host, like glibc."""

import os
from pathlib import Path
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]


class AppImageHostGraphicsTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.appdir = Path(directory.name) / "AppDir"
        (self.appdir / "usr/lib").mkdir(parents=True)

    def run_shell(self, commands):
        env = dict(os.environ, APPDIR_FIXTURE=str(self.appdir))
        return subprocess.run(["bash", "-c", "source scripts/ci/_common.sh\n" + commands],
                              cwd=ROOT, env=env, capture_output=True, text=True)

    def test_gbm_and_libdrm_are_left_to_the_host_like_glibc(self):
        for soname in ("libgbm.so.1", "libdrm.so.2", "libc.so.6"):
            with self.subTest(soname=soname):
                result = self.run_shell(f"linux_appimage_ignored_needed_library {soname}")
                self.assertEqual(result.returncode, 0, result.stderr)
        # Their siblings are still bundled.
        for soname in ("libdrm_amdgpu.so.1", "libgtk-3.so.0", "libEGL.so.1"):
            with self.subTest(soname=soname):
                self.assertEqual(self.run_shell(f"linux_appimage_ignored_needed_library {soname}").returncode, 1)

    def test_an_appimage_bundling_a_host_library_fails_verification(self):
        result = self.run_shell('verify_linux_appdir_runtime_closure "$APPDIR_FIXTURE"')
        self.assertEqual(result.returncode, 0, result.stderr)

        for soname in ("libgbm.so.1", "libdrm.so.2"):
            with self.subTest(soname=soname):
                library = self.appdir / "usr/lib" / soname
                library.write_bytes(b"")
                result = self.run_shell('verify_linux_appdir_runtime_closure "$APPDIR_FIXTURE"')
                self.assertEqual(result.returncode, 1)
                self.assertIn(f"AppImage bundles usr/lib/{soname}, which the host must supply.", result.stderr)
                library.unlink()


if __name__ == "__main__":
    unittest.main()
