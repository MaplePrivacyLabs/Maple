#!/usr/bin/env python3
"""Exercise AppImage runtime constants, repair, and portable library loading."""

import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
PREFIX = "/nix/store/" + "a" * 32


@unittest.skipUnless(sys.platform == "linux" and shutil.which("cc") and shutil.which("patchelf"),
                     "needs Linux ELF build tools")
class RuntimePathTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.app = self.root / "AppDir"
        self.lib = self.app / "usr/lib"
        self.lib.mkdir(parents=True)
        self.closure = self.root / "closure"
        (self.closure / "lib").mkdir(parents=True)
        (self.root / "store-paths").write_text(str(self.closure) + "\n")
        for name in ("libGLESv2.so.2", "libGL.so.1", "libsoup-3.0.so.0"):
            self.compile(self.closure / "lib" / name, 'int fixture(void) { return 42; }')

    def compile(self, output, source):
        subprocess.run(["cc", "-shared", "-fPIC", "-x", "c", "-", "-o", str(output)],
                       input=source, text=True, check=True, capture_output=True)
        subprocess.run(["patchelf", "--remove-rpath", str(output)], check=True)

    def shell(self, command):
        return subprocess.run(["bash", "-c", 'source scripts/ci/_common.sh\n' + command],
                              cwd=ROOT, capture_output=True, text=True,
                              env=dict(os.environ, FIXTURE=str(self.app),
                                       MAPLE_NIX_LINUX_CLOSURE_INFO=str(self.root),
                                       SOURCE_DATE_EPOCH="1234567890"))

    def test_rejects_live_paths_and_repairs_to_portable_loads(self):
        epoxy = PREFIX + "-libglvnd-1/lib"
        soup = PREFIX + "-libsoup/lib/libsoup-3.0.so.0"
        self.compile(self.lib / "libepoxy.so.0", f'''
#include <dlfcn.h>
#include <stdio.h>
const char *prefix = "{epoxy}";
const char *soup = "{soup}";
int probe(void) {{
    char path[256];
    snprintf(path, sizeof path, "%s/libGLESv2.so.2", prefix);
    return dlopen(path, RTLD_NOW) && dlopen(soup, RTLD_NOW);
}}
''')
        result = self.shell('verify_linux_appdir_portable_runtime_paths "$FIXTURE"')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(epoxy, result.stderr)
        self.assertIn(soup, result.stderr)
        result = self.shell('''
# Reject premature conversion to the host's distributable interpreter.
sanitize_linux_package_executable() { return 19; }
install_linux_appimage_string_patcher "$FIXTURE" build
repair_linux_appdir_runtime_library_paths "$FIXTURE"
# Keep the test helper runnable while removing its build-host RPATH.
sanitize_linux_package_executable() { patchelf --remove-rpath "$1"; }
sanitize_linux_package_executable "$FIXTURE/usr/libexec/maple-webkit-runtime/maple-replace-strings"
sanitize_linux_appdir_library_rpaths "$FIXTURE"
verify_linux_appdir_portable_runtime_paths "$FIXTURE"
''')
        self.assertEqual(result.returncode, 0, result.stderr)
        result = subprocess.run(
            [shutil.which("python3"), "-c",
             "import ctypes,sys; assert ctypes.CDLL(sys.argv[1]).probe() == 1",
             str(self.lib / "libepoxy.so.0")],
            env=dict(os.environ, LD_LIBRARY_PATH=str(self.lib)), capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        (self.lib / "libGLESv2.so.2").unlink()
        result = self.shell('verify_linux_appdir_runtime_closure "$FIXTURE"')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("libepoxy dlopen library libGLESv2.so.2", result.stderr)

    def test_ignores_source_locations_and_obsolete_dynamic_strings(self):
        path = PREFIX + "-sqlite/lib/libsqlite3.so"
        self.compile(self.lib / "libfixture.so",
                     f'const char *source = "{PREFIX}-source/src/file.c";')
        elf = self.lib / "libfixture.so"
        subprocess.run(["patchelf", "--add-needed", path, str(elf)], check=True)
        subprocess.run(["patchelf", "--replace-needed", path, "libsqlite3.so", str(elf)],
                       check=True)
        self.assertIn(path.encode(), elf.read_bytes())
        result = self.shell('verify_linux_appdir_portable_runtime_paths "$FIXTURE"')
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_missing_dlopen_dependency_fails_repair(self):
        self.compile(self.lib / "libfixture.so",
                     f'const char *path = "{PREFIX}-missing/lib/libmissing.so.1";')
        result = self.shell('''
sanitize_linux_package_executable() { patchelf --remove-rpath "$1"; }
install_linux_appimage_string_patcher "$FIXTURE" build
repair_linux_appdir_runtime_library_paths "$FIXTURE"
''')
        self.assertNotEqual(result.returncode, 0)


if __name__ == "__main__":
    unittest.main()
