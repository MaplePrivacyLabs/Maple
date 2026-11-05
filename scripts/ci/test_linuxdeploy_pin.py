#!/usr/bin/env python3
"""The Linux AppImage build runs only the pinned linuxdeploy that tauri-cli expects."""

import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
FRONTEND = ROOT / "apps/maple-research/frontend"


class LinuxdeployPinTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.tools = self.root / "tauri/target/.tauri"
        self.tools.mkdir(parents=True)
        self.cli = self.root / "frontend/node_modules/@tauri-apps/cli/package.json"
        self.cli.parent.mkdir(parents=True)

    def run_shell(self, commands):
        return subprocess.run(
            ["bash", "-c", "source scripts/ci/_common.sh\n"
             'TAURI_DIR="$FIXTURE_ROOT/tauri"\n'
             'FRONTEND_DIR="$FIXTURE_ROOT/frontend"\n'
             "linuxdeploy_tools_arch() { echo x86_64; }\n" + commands],
            cwd=ROOT, env=dict(os.environ, FIXTURE_ROOT=str(self.root)), capture_output=True, text=True)

    def pinned(self, name):
        return self.run_shell(f'printf "%s" "${name}"').stdout

    def test_the_pin_names_the_cli_this_research_app_installs(self):
        package = json.loads((FRONTEND / "package.json").read_text())
        self.assertEqual(package["devDependencies"]["@tauri-apps/cli"], self.pinned("TAURI_LINUXDEPLOY_CLI"))
        self.assertRegex(self.pinned("TAURI_LINUXDEPLOY_HASH"), r"\A[0-9a-f]{7,}\Z")

    def test_bundling_needs_the_cli_the_hash_was_read_from(self):
        self.cli.write_text(json.dumps({"version": self.pinned("TAURI_LINUXDEPLOY_CLI")}))
        result = self.run_shell("require_tauri_linuxdeploy_hash")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.cli.write_text(json.dumps({"version": "9.9.9"}))
        result = self.run_shell("require_tauri_linuxdeploy_hash")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("TAURI_LINUXDEPLOY_HASH", result.stderr)
        self.cli.unlink()
        self.assertNotEqual(self.run_shell("require_tauri_linuxdeploy_hash").returncode, 0)

    def test_a_linuxdeploy_fetched_by_tauri_fails_the_build(self):
        wrapper = f"linuxdeploy-{self.pinned('TAURI_LINUXDEPLOY_HASH')}-x86_64.AppImage"
        # Maple's own tools: the wrapper, the real AppImage, the AppImage plugin wrapper
        # and the unhashed name earlier tauri-cli versions used.
        for name in (wrapper, "linuxdeploy-x86_64.real.AppImage", "linuxdeploy-plugin-appimage.AppImage",
                     "linuxdeploy-x86_64.AppImage"):
            (self.tools / name).write_text("fixture")
        result = self.run_shell("verify_tauri_used_pinned_linuxdeploy")
        self.assertEqual(result.returncode, 0, result.stderr)
        (self.tools / "linuxdeploy-1234567-x86_64.AppImage").write_text("downloaded")
        result = self.run_shell("verify_tauri_used_pinned_linuxdeploy")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("linuxdeploy-1234567-x86_64.AppImage", result.stderr)


if __name__ == "__main__":
    unittest.main()
