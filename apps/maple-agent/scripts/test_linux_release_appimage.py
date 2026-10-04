#!/usr/bin/env python3
"""Hermetic tests for the AppImage closure/launcher boundary, without a build."""

import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest
from unittest import mock


spec = importlib.util.spec_from_file_location(
    "linux_release_appimage", Path(__file__).with_name("linux-release-appimage.py")
)
packaging = importlib.util.module_from_spec(spec)
spec.loader.exec_module(packaging)

HEADER = b"\x7fELF\x02\x01" + b"\x00" * 12 + b"\x3e\x00"
METADATA = {
    "channel": "dev",
    "app_name": "Maple Agent Dev",
    "bundle_id": "cloud.opensecret.maple.agent.dev",
    "version": "0.1.0",
    "build_number": "1",
}


def write_elf(path, needed=(), rpath="", interpreter="", requires=(), supplies=(), version_providers=None):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(
        HEADER + json.dumps({
            "needed": list(needed), "rpath": rpath, "interpreter": interpreter,
            "requires": list(requires), "supplies": list(supplies),
            "version_providers": version_providers if version_providers is not None else ({"libc.so.6": list(requires)} if requires else {}),
        }, sort_keys=True).encode()
    )
    path.chmod(0o755)


class ElfTools:
    """Mock patchelf/readelf, keeping metadata with each copied ELF fixture."""

    def __call__(self, *args, check=True, env=None):
        command, option, path = args[0], args[1], Path(args[-1])
        state = json.loads(path.read_bytes()[20:])
        stdout = ""
        returncode = 0
        if command == "readelf":
            if state["supplies"]:
                stdout += "Version definition section\n"
                stdout += "\n".join(f"Name: {version}" for version in state["supplies"])
            if state["version_providers"]:
                stdout += "\nVersion needs section\n"
                for provider, versions in state["version_providers"].items():
                    stdout += f"  000000: Version: 1 File: {provider} Cnt: {len(versions)}\n"
                    stdout += "\n".join(f"  0x0010: Name: {version} Flags: none Version: 2" for version in versions)
                    stdout += "\n"
        elif option == "--print-needed":
            stdout = "\n".join(state["needed"])
        elif option == "--print-rpath":
            stdout = state["rpath"]
        elif option == "--print-interpreter":
            stdout = state["interpreter"]
            returncode = 0 if stdout else 1
        elif option == "--replace-needed":
            state["needed"] = [args[3] if item == args[2] else item for item in state["needed"]]
            if args[2] in state["version_providers"]:
                state["version_providers"][args[3]] = state["version_providers"].pop(args[2])
        elif option == "--set-rpath":
            state["rpath"] = args[2]
        elif option == "--set-interpreter":
            state["interpreter"] = args[2]
        else:
            raise AssertionError(f"Unexpected tool command: {args}")
        if option.startswith("--set") or option == "--replace-needed":
            path.write_bytes(HEADER + json.dumps(state, sort_keys=True).encode())
        return subprocess.CompletedProcess(args, returncode, stdout, "")


class AppImageTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.runtime = self.root / "runtime"
        self.glibc = self.root / "glibc"
        self.store_paths = self.root / "store-paths"
        self.store_paths.write_text(f"{self.runtime}\n{self.glibc}\n")
        self.binary = self.root / "native/maple-agent"
        write_elf(
            self.glibc / "lib/libc.so.6", interpreter="/nix/store/glibc/lib/ld-linux-x86-64.so.2",
            supplies=("GLIBC_2.2.5", "GLIBC_2.42", "GLIBC_PRIVATE"),
        )
        write_elf(self.glibc / "lib" / packaging.LOADER)
        for soname in packaging.DLOPEN_LIBRARIES:
            write_elf(self.runtime / "lib" / soname, needed=(str(self.glibc / "lib/libc.so.6"),))
        write_elf(self.runtime / "lib/libalpha.so.1", needed=("libbeta.so.1",), rpath=str(self.runtime / "lib"))
        write_elf(self.runtime / "lib/libbeta.so.1", needed=(str(self.glibc / "lib/libc.so.6"),))
        write_elf(
            self.binary, needed=(str(self.runtime / "lib/libalpha.so.1"), "libc.so.6"),
            rpath=str(self.glibc / "lib"), interpreter="/nix/store/glibc/lib/ld-linux-x86-64.so.2",
            requires=("GLIBC_2.42",),
        )
        self.component = self.root / "component"
        icons = self.component / "app/packaging"
        icons.mkdir(parents=True)
        (icons / "maple-agent.png").write_bytes(b"prod-icon")
        (icons / "maple-agent-dev.png").write_bytes(b"dev-icon")
        self.tools = mock.patch.object(packaging, "run", ElfTools())
        self.tools.start()
        self.addCleanup(self.tools.stop)

    def stage(self):
        appdir = self.root / "AppDir"
        packaging.stage_closure(appdir, self.binary, packaging.Closure(self.store_paths), self.glibc)
        packaging.stage_metadata(appdir, self.component, METADATA)
        packaging.normalize_public_permissions(appdir)
        return appdir

    def test_transitive_and_dlopen_closure_is_relative_and_matches_libc_loader(self):
        appdir = self.stage()
        result = packaging.audit(appdir)
        self.assertTrue(result["bundled_glibc"])
        self.assertEqual(result["required_glibc_versions"], ["GLIBC_2.42"])
        executable = appdir / "usr/bin/maple-agent"
        self.assertEqual(packaging.needed(executable), ["libalpha.so.1", "libc.so.6"])
        self.assertEqual(packaging.interpreter(executable), packaging.INTERPRETER)
        self.assertTrue((appdir / "usr/lib/libbeta.so.1").is_file())
        self.assertEqual((appdir / f"{METADATA['bundle_id']}.png").read_bytes(), b"dev-icon")

    def test_missing_dynamic_library_fails_closed(self):
        (self.runtime / "lib/libvulkan.so.1").unlink()
        with self.assertRaisesRegex(packaging.PackagingError, "Missing pinned runtime library: libvulkan"):
            self.stage()

    def test_unwinder_seed_preserves_original_main_binary_rpath(self):
        # The pinned glibc closure also contains bootstrap xgcc's libgcc. The
        # main executable's original RPATH identifies the full-GCC runtime
        # selected by its linker rather than an arbitrary closure candidate.
        alternative = self.root / "bootstrap-libgcc"
        self.store_paths.write_text(self.store_paths.read_text() + f"{alternative}\n")
        write_elf(alternative / "lib/libgcc_s.so.1", requires=("GLIBC_2.2.5",))
        write_elf(self.binary, needed=("libgcc_s.so.1", "libc.so.6"), rpath=f"{self.runtime}/lib:{self.glibc}/lib")
        appdir = self.stage()
        packaging.audit(appdir)
        selected = json.loads((appdir / "usr/lib/libgcc_s.so.1").read_bytes()[20:])
        self.assertEqual(selected["requires"], [])
        self.assertEqual(selected["needed"], ["libc.so.6"])

    def test_unwinder_seed_without_linker_selection_rejects_ambiguity(self):
        alternative = self.root / "bootstrap-libgcc"
        self.store_paths.write_text(self.store_paths.read_text() + f"{alternative}\n")
        write_elf(alternative / "lib/libgcc_s.so.1", requires=("GLIBC_2.2.5",))
        with self.assertRaisesRegex(packaging.PackagingError, "Ambiguous pinned runtime library: libgcc_s.so.1"):
            self.stage()

    def test_missing_transitive_library_fails_closed(self):
        (self.runtime / "lib/libbeta.so.1").unlink()
        with self.assertRaisesRegex(packaging.PackagingError, "Missing pinned runtime library: libbeta"):
            self.stage()

    def test_dependency_outside_pinned_closure_is_rejected(self):
        outsider = self.root / "host/liboutside.so.1"
        write_elf(outsider)
        write_elf(self.binary, needed=(str(outsider),))
        with self.assertRaisesRegex(packaging.PackagingError, "outside the pinned runtime closure"):
            self.stage()

    def test_conflicting_soname_versions_are_rejected(self):
        alternative = self.root / "alternative"
        self.store_paths.write_text(self.store_paths.read_text() + f"{alternative}\n")
        write_elf(alternative / "lib/libbeta.so.1", requires=("GLIBC_2.2.5",))
        write_elf(self.runtime / "lib/libgamma.so.1", needed=("libbeta.so.1",), rpath=str(alternative / "lib"))
        write_elf(self.binary, needed=("libalpha.so.1", "libgamma.so.1", "libc.so.6"), rpath=str(self.runtime / "lib"))
        with self.assertRaisesRegex(packaging.PackagingError, "Conflicting libraries share SONAME libbeta"):
            self.stage()

    def test_audit_rejects_escape_and_absolute_needed(self):
        appdir = self.stage()
        escape = appdir / "usr/lib/escape"
        escape.symlink_to(self.binary)
        with self.assertRaisesRegex(packaging.PackagingError, "symlink escapes"):
            packaging.audit(appdir)
        escape.unlink()
        write_elf(appdir / "usr/lib/libbeta.so.1", needed=("/nix/store/forgotten/libc.so.6",), rpath="$ORIGIN")
        with self.assertRaisesRegex(packaging.PackagingError, "Absolute DT_NEEDED"):
            packaging.audit(appdir)

    def test_audit_rejects_incompatible_glibc_versions(self):
        appdir = self.stage()
        write_elf(appdir / "usr/bin/maple-agent", needed=("libc.so.6",), rpath="$ORIGIN/../lib", requires=("GLIBC_999.0",))
        with self.assertRaisesRegex(packaging.PackagingError, "does not supply required versions"):
            packaging.audit(appdir)

    def test_glibc_version_is_checked_against_its_declared_libm_provider(self):
        # Exact pinned glibc 2.42 ABI tables expose GLIBC_2.40 in libm, while
        # libc does not define that version label. The provider still satisfies
        # this valid dependency regardless of libc's distinct definition set.
        appdir = self.stage()
        write_elf(appdir / "usr/lib/libm.so.6", rpath="$ORIGIN", supplies=("GLIBC_2.40",))
        write_elf(appdir / "usr/lib/libbeta.so.1", needed=("libm.so.6",), rpath="$ORIGIN", version_providers={"libm.so.6": ["GLIBC_2.40"]})
        self.assertIn("GLIBC_2.40", packaging.audit(appdir)["required_glibc_versions"])
        # The presence of this version in libm cannot satisfy a need declared
        # against libc. A global union would incorrectly accept this payload.
        write_elf(appdir / "usr/lib/libbeta.so.1", needed=("libc.so.6",), rpath="$ORIGIN", version_providers={"libc.so.6": ["GLIBC_2.40"]})
        with self.assertRaisesRegex(packaging.PackagingError, "Bundled libc.so.6 does not supply.*GLIBC_2.40"):
            packaging.audit(appdir)

    def test_provider_version_needs_are_not_version_definitions(self):
        appdir = self.stage()
        write_elf(appdir / "usr/lib/libc.so.6", rpath="$ORIGIN", supplies=("GLIBC_2.42", "GLIBC_2.40"))
        write_elf(appdir / "usr/lib/libm.so.6", needed=("libc.so.6",), rpath="$ORIGIN", supplies=("GLIBC_2.39",), version_providers={"libc.so.6": ["GLIBC_2.40"]})
        write_elf(appdir / "usr/lib/libbeta.so.1", needed=("libm.so.6",), rpath="$ORIGIN", version_providers={"libm.so.6": ["GLIBC_2.40"]})
        with self.assertRaisesRegex(packaging.PackagingError, "Bundled libm.so.6 does not supply.*GLIBC_2.40"):
            packaging.audit(appdir)

    def test_matching_gconv_modules_use_relative_rpath(self):
        write_elf(self.glibc / "lib/gconv/EXAMPLE.so", needed=("libc.so.6",))
        (self.glibc / "lib/gconv/gconv-modules").write_text("module EXAMPLE// INTERNAL EXAMPLE 1\n")
        (self.glibc / "lib/gconv/gconv-modules.cache").write_bytes(b"/nix/store/cache")
        appdir = self.stage()
        packaging.audit(appdir)
        module = appdir / "usr/lib/gconv/EXAMPLE.so"
        self.assertEqual(packaging.run("patchelf", "--print-rpath", module).stdout, "$ORIGIN/..")
        self.assertFalse((appdir / "usr/lib/gconv/gconv-modules.cache").exists())

    def test_failed_staging_publishes_no_output(self):
        (self.runtime / "lib/libbeta.so.1").unlink()
        output = self.root / "must-not-exist.AppDir"
        variables = {f"MAPLE_PACKAGE_{key.upper()}": value for key, value in METADATA.items()}
        variables.update({
            "MAPLE_AGENT_LINUX_CLOSURE_INFO": str(self.root),
            "MAPLE_AGENT_LINUX_GLIBC": str(self.glibc),
            "SOURCE_DATE_EPOCH": "12345",
        })
        with mock.patch.dict(os.environ, variables), mock.patch.object(packaging.sys, "argv", ["helper", "--stage-only", str(self.binary), str(output)]):
            with self.assertRaises(packaging.PackagingError):
                packaging.main()
        self.assertFalse(output.exists())
        self.assertFalse(output.with_name(output.name + ".runtime-audit.json").exists())

    def test_squashfs_uses_explicit_epoch_without_conflicting_environment(self):
        output = self.root / "release.AppImage"
        tools = self.root / "appimage-tools"
        tools.mkdir()
        (tools / "runtime-x86_64").write_bytes(b"pinned-runtime")
        variables = {f"MAPLE_PACKAGE_{key.upper()}": value for key, value in METADATA.items()}
        variables.update({
            "MAPLE_AGENT_LINUX_CLOSURE_INFO": str(self.root),
            "MAPLE_AGENT_LINUX_GLIBC": str(self.glibc),
            "MAPLE_AGENT_APPIMAGE_TOOLS": str(tools),
            "SOURCE_DATE_EPOCH": "12345",
        })
        squashfs_calls = []

        def tool_run(*args, check=True, env=None):
            if args[0] == "mksquashfs":
                environment = os.environ if env is None else env
                if "SOURCE_DATE_EPOCH" in environment:
                    raise packaging.PackagingError("SOURCE_DATE_EPOCH and command line options can't be used at the same time to set timestamp(s)")
                squashfs_calls.append(args)
                Path(args[2]).write_bytes(b"squashfs-payload")
                return subprocess.CompletedProcess(args, 0, "", "")
            if Path(args[0]).name == "AppRun":
                appdir, desktop, icon = map(Path, (args[2], args[4], args[6]))
                for source, target in ((desktop, appdir / "usr/share/applications" / desktop.name), (icon, appdir / "usr/share/icons" / icon.name)):
                    target.parent.mkdir(parents=True, exist_ok=True)
                    shutil.copy2(source, target)
                return subprocess.CompletedProcess(args, 0, "", "")
            return ElfTools()(*args, check=check, env=env)

        def extract(source, destination):
            if source.name == "linuxdeploy-x86_64.AppImage":
                destination.mkdir()
            else:
                shutil.copytree(source.parent / "MapleAgent.AppDir", destination)

        with mock.patch.dict(os.environ, variables), mock.patch.object(packaging, "run", tool_run), mock.patch.object(packaging, "extract_appimage", extract), mock.patch.object(packaging.sys, "argv", ["helper", str(self.binary), str(output)]), mock.patch("builtins.print"):
            packaging.main()
            self.assertEqual(os.environ["SOURCE_DATE_EPOCH"], "12345")
        self.assertEqual(len(squashfs_calls), 1)
        args = squashfs_calls[0]
        self.assertEqual(args[args.index("-all-time") + 1], 12345)
        self.assertEqual(args[args.index("-mkfs-time") + 1], 12345)
        self.assertEqual(output.read_bytes(), b"pinned-runtimesquashfs-payload")
        self.assertTrue(output.with_name(output.name + ".runtime-audit.json").is_file())

    def test_launcher_library_path_does_not_escape_to_child_environment(self):
        appdir = self.root / "launch with spaces"
        (appdir / "usr/lib").mkdir(parents=True)
        (appdir / "usr/lib/gconv").mkdir()
        launcher = appdir / "AppRun"
        launcher.write_text(packaging.APPRUN)
        launcher.chmod(0o755)
        loader = appdir / "usr/lib" / packaging.LOADER
        loader.write_text('#!/bin/sh\nprintf "library_env=%s\\ngconv_env=%s\\n" "${LD_LIBRARY_PATH-unset}" "${GCONV_PATH-unset}"\nprintf "%s\\n" "$@"\n')
        loader.chmod(0o755)
        environment = os.environ.copy()
        environment.pop("LD_LIBRARY_PATH", None)
        environment.pop("GCONV_PATH", None)
        result = subprocess.run([str(launcher), "--version"], text=True, capture_output=True, env=environment, check=True)
        self.assertTrue(result.stdout.startswith("library_env=unset\ngconv_env=unset\n--inhibit-cache\n--library-path\n"))
        self.assertIn(str(appdir / "usr/lib"), result.stdout)
        self.assertTrue(result.stdout.endswith("--version\n"))

    def test_private_outer_umask_produces_public_payload_modes(self):
        previous = os.umask(0o077)
        try:
            appdir = self.stage()
        finally:
            os.umask(previous)
        packaging.audit(appdir)
        self.assertEqual(appdir.stat().st_mode & 0o777, 0o755)
        self.assertEqual((appdir / "usr/share/maple-agent/package-metadata.json").stat().st_mode & 0o777, 0o644)
        self.assertEqual((appdir / "AppRun").stat().st_mode & 0o777, 0o755)
        (appdir / "usr/share/maple-agent/package-metadata.json").chmod(0o600)
        with self.assertRaisesRegex(packaging.PackagingError, "public package file permissions"):
            packaging.audit(appdir)


if __name__ == "__main__":
    unittest.main()
