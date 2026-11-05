#!/usr/bin/env python3
"""Exercise Windows linker selection and the actual Cargo smoke boundary."""

import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

import agent_windows


class AgentWindowsTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name).resolve()
        self.toolset = self.root / "Visual Studio/VC/Tools/MSVC/14.51"
        self.compiler = self.toolset / "bin/HostX64/x64/cl.exe"
        self.compiler.parent.mkdir(parents=True)
        self.compiler.write_bytes(b"MSVC compiler fixture")
        self.linker = self.compiler.with_name("link.exe")
        self.linker.write_bytes(b"MSVC linker fixture")
        self.github_env = self.root / "github-env"
        self.environment = {
            "VCToolsInstallDir": str(self.toolset), "GITHUB_ENV": str(self.github_env),
            "RUSTFLAGS": "-D warnings", "CFLAGS": "/guard:cf",
            "CARGO_TARGET_DIR": str(self.root / "unrelated-target"),
            "CARGO_BUILD_BUILD_DIR": str(self.root / "unrelated-build"),
        }
        self.identity = f"rustc 1.98.0 (fixture)\nhost: {agent_windows.HOST}\nrelease: 1.98.0\n"
        self.calls = []
        self.fail_build = False
        self.fail_run = False
        self.create_executable = True
        patches = (
            patch.dict(os.environ, self.environment, clear=True),
            patch.object(agent_windows.platform, "system", return_value="Windows"),
            patch.object(agent_windows.shutil, "which", side_effect=self.which),
            patch.object(agent_windows.subprocess, "run", side_effect=self.run_tool),
            patch("sys.argv", ["agent_windows.py"]),
        )
        for selected in patches:
            selected.start()
            self.addCleanup(selected.stop)

    def which(self, name):
        return str({"cl.exe": self.compiler}.get(name, self.root / name))

    def run_tool(self, arguments, **options):
        self.calls.append((arguments, options))
        if arguments[1:] == ["-vV"]:
            return subprocess.CompletedProcess(arguments, 0, stdout=self.identity, stderr="")
        # Export must remain pending until both Cargo compilation and native
        # startup succeed; a broken host build-script linker cannot be saved.
        self.assertFalse(self.github_env.exists())
        environment = options["env"]
        self.assertEqual(environment[agent_windows.target_identity(environment.get("MAPLE_WINDOWS_ARCH", "x86_64"))[1]], str(self.linker))
        self.assertEqual(environment["RUSTFLAGS"], self.environment["RUSTFLAGS"])
        self.assertEqual(environment["CFLAGS"], self.environment["CFLAGS"])
        self.assertTrue(options["check"])
        self.assertNotIn("shell", options)
        self.assertNotIn("cwd", options)
        if len(arguments) > 1 and arguments[1] == "build":
            self.assertIn("--offline", arguments)
            self.assertIn("--verbose", arguments)
            self.assertNotIn("--target", arguments)
            manifest = Path(arguments[arguments.index("--manifest-path") + 1])
            fixture = manifest.parent
            self.assertIn('[workspace]', manifest.read_text())
            self.assertIn(f"cargo:rustc-env={agent_windows.MARKER}=ok", (fixture / "build.rs").read_text())
            self.assertIn(f'env!("{agent_windows.MARKER}")', (fixture / "src/main.rs").read_text())
            self.assertEqual(Path(environment["CARGO_TARGET_DIR"]), fixture / "target")
            self.assertEqual(Path(environment["CARGO_BUILD_BUILD_DIR"]), fixture / "build")
            if self.fail_build:
                raise subprocess.CalledProcessError(1, arguments)
            if self.create_executable:
                executable = fixture / "target/debug/maple-windows-linker-smoke.exe"
                executable.parent.mkdir(parents=True)
                executable.write_bytes(b"native smoke fixture")
        else:
            self.assertEqual(Path(arguments[0]).name, "maple-windows-linker-smoke.exe")
            self.assertTrue(Path(arguments[0]).is_file())
            if self.fail_run:
                raise subprocess.CalledProcessError(1, arguments)
        return subprocess.CompletedProcess(arguments, 0)

    def test_success_links_build_script_and_binary_before_export(self):
        agent_windows.main()
        self.assertEqual(len(self.calls), 3)
        self.assertEqual(self.github_env.read_text(), f"{agent_windows.LINKER_ENV}={self.linker}\n")
        self.assertEqual(os.environ[agent_windows.LINKER_ENV], str(self.linker))
        manifest = Path(self.calls[1][0][-1])
        self.assertFalse(manifest.parent.exists())
        self.assertFalse((self.root / "unrelated-target").exists())
        self.assertFalse((self.root / "unrelated-build").exists())

    def test_arm64_native_host_links_and_exports_only_arm64_linker(self):
        os.environ["MAPLE_WINDOWS_ARCH"] = "aarch64"
        self.identity = "rustc fixture\nhost: aarch64-pc-windows-msvc\n"
        agent_windows.main()
        linker_env = agent_windows.target_identity("aarch64")[1]
        self.assertEqual(self.github_env.read_text(), f"{linker_env}={self.linker}\n")
        self.assertNotIn(agent_windows.LINKER_ENV, os.environ)

    def test_architecture_mismatch_and_unknown_target_fail_before_smoke(self):
        os.environ["MAPLE_WINDOWS_ARCH"] = "aarch64"
        with self.assertRaisesRegex(ValueError, "requires Rust host aarch64"):
            agent_windows.main()
        self.assertEqual(len(self.calls), 1)
        with self.assertRaisesRegex(ValueError, "unsupported native Windows"):
            agent_windows.target_identity("arm64ec")

    def test_wrong_rust_host_fails_before_linker_export_or_smoke(self):
        self.identity = "rustc fixture\nhost: aarch64-pc-windows-msvc\n"
        with self.assertRaisesRegex(ValueError, "requires Rust host"):
            agent_windows.main()
        self.assertEqual(len(self.calls), 1)
        self.assertNotIn(agent_windows.LINKER_ENV, os.environ)
        self.assertFalse(self.github_env.exists())

    def test_non_windows_fails_before_any_probe(self):
        with patch.object(agent_windows.platform, "system", return_value="Linux"):
            with self.assertRaisesRegex(ValueError, "requires Windows"):
                agent_windows.main()
        self.assertEqual(self.calls, [])

    def test_missing_github_env_cannot_claim_persistent_selection(self):
        del os.environ["GITHUB_ENV"]
        with self.assertRaisesRegex(ValueError, "GITHUB_ENV is required"):
            agent_windows.main()
        self.assertEqual(self.calls, [])

    def test_failed_host_build_does_not_export(self):
        self.fail_build = True
        with self.assertRaises(subprocess.CalledProcessError):
            agent_windows.main()
        self.assertEqual(len(self.calls), 2)
        self.assertFalse(self.github_env.exists())
        self.assertFalse(Path(self.calls[1][0][-1]).parent.exists())

    def test_failed_native_startup_does_not_export(self):
        self.fail_run = True
        with self.assertRaises(subprocess.CalledProcessError):
            agent_windows.main()
        self.assertEqual(len(self.calls), 3)
        self.assertFalse(self.github_env.exists())

    def test_missing_native_output_fails_closed(self):
        self.create_executable = False
        with self.assertRaisesRegex(ValueError, "expected native Windows"):
            agent_windows.main()
        self.assertEqual(len(self.calls), 2)
        self.assertFalse(self.github_env.exists())

    def test_selected_toolset_environment_is_case_insensitive(self):
        self.assertEqual(agent_windows.selected_linker({"VCTOOLSINSTALLDIR": str(self.toolset)}), self.linker)

    def test_compiler_outside_selected_toolset_is_rejected(self):
        with self.assertRaisesRegex(ValueError, "outside the selected"):
            agent_windows.selected_linker({"VCToolsInstallDir": str(self.root / "different-toolset")})

    def test_missing_toolset_or_compiler_is_rejected(self):
        with self.assertRaisesRegex(ValueError, "MSVC developer environment"):
            agent_windows.selected_linker({})
        with patch.object(agent_windows.shutil, "which", return_value=None):
            with self.assertRaisesRegex(ValueError, "required Windows build tool"):
                agent_windows.selected_linker(self.environment)

    def test_missing_empty_or_unreadable_linker_is_rejected(self):
        self.linker.unlink()
        with self.assertRaisesRegex(ValueError, "MSVC link.exe is missing"):
            agent_windows.selected_linker(self.environment)
        self.linker.touch()
        with self.assertRaisesRegex(ValueError, "link.exe is empty"):
            agent_windows.selected_linker(self.environment)
        self.linker.write_bytes(b"linker")
        with patch.object(agent_windows.Path, "open", side_effect=PermissionError("unreadable linker")):
            with self.assertRaises(PermissionError):
                agent_windows.selected_linker(self.environment)


if __name__ == "__main__":
    unittest.main()
