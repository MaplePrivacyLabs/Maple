#!/usr/bin/env python3
"""Hermetic tests for the Linux payload audit and launcher boundary, without a build."""

import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest import mock



spec = importlib.util.spec_from_file_location(
    "linux_release_appimage", Path(__file__).with_name("linux-release-appimage.py")
)
packaging = importlib.util.module_from_spec(spec)
spec.loader.exec_module(packaging)
NATIVE_RUN = packaging.run

HEADER = b"\x7fELF\x02\x01" + b"\x00" * 12 + b"\x3e\x00"
METADATA = {
    "channel": "dev",
    "app_name": "Maple Work Dev",
    "bundle_id": "cloud.opensecret.maple.agent.dev",
    "version": "0.1.0",
    "build_number": "1",
}


def write_elf(path, needed=("libc.so.6",), rpath="", interpreter="/lib64/ld-linux-x86-64.so.2", requires=("GLIBC_2.2.5",)):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(
        HEADER + json.dumps({
            "needed": list(needed), "rpath": rpath, "interpreter": interpreter, "requires": list(requires),
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
            stdout = "Version needs section\n  000000: Version: 1 File: libc.so.6 Cnt: 1\n"
            stdout += "\n".join(f"  0x0010: Name: {version} Flags: none Version: 2" for version in state["requires"])
        elif option == "--print-needed":
            stdout = "\n".join(state["needed"])
        elif option == "--print-rpath":
            stdout = state["rpath"]
        elif option == "--print-interpreter":
            stdout = state["interpreter"]
            returncode = 0 if stdout else 1
        else:
            raise AssertionError(f"Unexpected tool command: {args}")
        return subprocess.CompletedProcess(args, returncode, stdout, "")


class AppImageTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.binary = self.root / "native/maple-agent"
        write_elf(
            self.binary,
            needed=("libxkbcommon.so.0", "libX11.so.6", "libasound.so.2", "libgcc_s.so.1", "libm.so.6", "libc.so.6"),
            requires=("GLIBC_2.2.5", "GLIBC_2.17", "GLIBC_2.31"),
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
        packaging.stage_payload(appdir, self.binary)
        packaging.stage_metadata(appdir, self.component, METADATA)
        packaging.normalize_public_permissions(appdir)
        return appdir

    def test_payload_uses_host_glibc_and_desktop_libraries_only(self):
        appdir = self.stage()
        result = packaging.audit(appdir)
        self.assertFalse(result["bundled_glibc"])
        self.assertEqual(result["interpreter"], packaging.INTERPRETER)
        self.assertEqual(result["glibc_floor"], "2.31")
        self.assertEqual(result["required_glibc_versions"], ["GLIBC_2.2.5", "GLIBC_2.17", "GLIBC_2.31"])
        self.assertIn("libxkbcommon.so.0", result["host_libraries"])
        elves = [path for path in appdir.rglob("*") if path.is_file() and packaging.elf(path)]
        self.assertEqual(elves, [appdir / "usr/bin/maple-agent"])
        compatibility = appdir / "usr/lib" / packaging.LOADER
        self.assertEqual(compatibility.read_text(), packaging.COMPATIBILITY_LOADER)
        self.assertEqual(compatibility.stat().st_mode & 0o777, 0o755)
        self.assertEqual((appdir / f"{METADATA['bundle_id']}.png").read_bytes(), b"dev-icon")

    def test_audit_rejects_glibc_above_the_floor_and_private_versions(self):
        appdir = self.stage()
        for version in ("GLIBC_2.34", "GLIBC_2.39", "GLIBC_PRIVATE", "GLIBC_ABI_DT_RELR"):
            write_elf(appdir / "usr/bin/maple-agent", requires=("GLIBC_2.2.5", version))
            with self.assertRaisesRegex(packaging.PackagingError, "newer than the 2.31 floor"):
                packaging.audit(appdir)

    def test_audit_rejects_libraries_a_desktop_may_not_provide(self):
        appdir = self.stage()
        write_elf(appdir / "usr/bin/maple-agent", needed=("libssl.so.3", "libc.so.6"))
        with self.assertRaisesRegex(packaging.PackagingError, "may not provide: \\['libssl.so.3'\\]"):
            packaging.audit(appdir)

    def test_audit_rejects_bundled_loader_paths_and_libraries(self):
        appdir = self.stage()
        payload = appdir / "usr/bin/maple-agent"
        write_elf(payload, rpath="$ORIGIN/../lib")
        with self.assertRaisesRegex(packaging.PackagingError, "runtime library path"):
            packaging.audit(appdir)
        write_elf(payload, interpreter="/nix/store/glibc/lib/ld-linux-x86-64.so.2")
        with self.assertRaisesRegex(packaging.PackagingError, "host loader"):
            packaging.audit(appdir)
        write_elf(payload)
        write_elf(appdir / "usr/lib/libxkbcommon.so.0")
        with self.assertRaisesRegex(packaging.PackagingError, "host libraries are never bundled"):
            packaging.audit(appdir)

    def test_audit_requires_reviewed_launchers(self):
        appdir = self.stage()
        compatibility = appdir / "usr/lib" / packaging.LOADER
        compatibility.write_text("#!/bin/sh\nexec /bin/true\n")
        with self.assertRaisesRegex(packaging.PackagingError, "compatibility launcher"):
            packaging.audit(appdir)
        compatibility.unlink()
        with self.assertRaisesRegex(packaging.PackagingError, "compatibility launcher"):
            packaging.audit(appdir)
        packaging.stage_payload(appdir, self.binary)
        (appdir / "AppRun").write_text(packaging.APPRUN + "export LD_LIBRARY_PATH=/nowhere\n")
        with self.assertRaisesRegex(packaging.PackagingError, "AppRun differs"):
            packaging.audit(appdir)
        (appdir / "AppRun").write_text(packaging.apprun_variants()[0])
        packaging.audit(appdir)

    def test_audit_rejects_escaping_symlink(self):
        appdir = self.stage()
        escape = appdir / "usr/lib/escape"
        escape.symlink_to(self.binary)
        with self.assertRaisesRegex(packaging.PackagingError, "symlink escapes"):
            packaging.audit(appdir)

    def test_failed_staging_publishes_no_output(self):
        write_elf(self.binary, needed=("libssl.so.3", "libc.so.6"))
        output = self.root / "must-not-exist.AppDir"
        variables = {f"MAPLE_PACKAGE_{key.upper()}": value for key, value in METADATA.items()}
        variables.update({
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

        with mock.patch.dict(os.environ, variables), mock.patch.object(packaging, "run", tool_run), mock.patch.object(packaging, "extract_appimage", extract), mock.patch.object(packaging.sys, "argv", ["helper", str(self.binary), str(output)]), mock.patch.object(packaging, "__file__", str(self.component / "scripts/linux-release-appimage.py")), mock.patch("builtins.print"):
            packaging.main()
            self.assertEqual(os.environ["SOURCE_DATE_EPOCH"], "12345")
        self.assertEqual(len(squashfs_calls), 1)
        args = squashfs_calls[0]
        self.assertEqual(args[args.index("-all-time") + 1], 12345)
        self.assertEqual(args[args.index("-mkfs-time") + 1], 12345)
        self.assertEqual(output.read_bytes(), b"pinned-runtimesquashfs-payload")
        self.assertTrue(output.with_name(output.name + ".runtime-audit.json").is_file())

    def fake_bundle(self):
        appdir = self.root / "launch with spaces"
        payload = self.root / "script-payload"
        payload.write_text('#!/bin/sh\nprintf "library_env=%s\\nappdir=%s\\n" "${LD_LIBRARY_PATH-unset}" "$APPDIR"\nprintf "%s\\n" "$@"\n')
        payload.chmod(0o755)
        packaging.stage_payload(appdir, payload)
        launcher = appdir / "AppRun"
        launcher.write_text(packaging.APPRUN)
        launcher.chmod(0o755)
        environment = os.environ.copy()
        environment.pop("LD_LIBRARY_PATH", None)
        return appdir.resolve(), environment

    @unittest.skipUnless(Path(packaging.INTERPRETER).exists(), "needs a glibc host loader")
    def test_launcher_runs_payload_directly_without_a_library_path(self):
        appdir, environment = self.fake_bundle()
        result = subprocess.run([str(appdir / "AppRun"), "--version", "two words"], text=True, capture_output=True, env=environment, check=True)
        self.assertEqual(result.stdout, f"library_env=unset\nappdir={appdir}\n--version\ntwo words\n")

    def test_compatibility_loader_accepts_old_loader_options_for_its_payload_only(self):
        appdir, environment = self.fake_bundle()
        compatibility = appdir / "usr/lib" / packaging.LOADER
        payload = appdir / "usr/bin/maple-agent"
        environment["APPDIR"] = "inherited"
        result = subprocess.run(
            [str(compatibility), "--inhibit-cache", "--library-path", f"{appdir}/usr/lib:/usr/lib", str(payload), "--internal-check", "/private/store"],
            text=True, capture_output=True, env=environment, check=True,
        )
        self.assertEqual(result.stdout, "library_env=unset\nappdir=inherited\n--internal-check\n/private/store\n")
        rejected = subprocess.run([str(compatibility), "/bin/echo", "escaped"], text=True, capture_output=True, env=environment)
        self.assertEqual(rejected.returncode, 127)
        self.assertNotIn("escaped", rejected.stdout)

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

    def test_extraction_preserves_archive_modes_and_private_parent_umask(self):
        tools = self.root / "extraction-tools"
        tools.mkdir()
        extractor = tools / "unsquashfs"
        # Match pinned nonroot unsquashfs's regular-file open behavior without
        # chmod after creation. Use this test's interpreter in the subprocess.
        extractor.write_text(f"#!{sys.executable}\n" + '''
import os
from pathlib import Path
import sys
if "-s" in sys.argv:
    sys.exit(0)
destination = Path(sys.argv[sys.argv.index("-d") + 1])
destination.mkdir(mode=0o755)
for name, mode in (("AppRun", 0o755), ("unsafe-executable", 0o777), ("unsafe-data", 0o666)):
    descriptor = os.open(destination / name, os.O_WRONLY | os.O_CREAT, mode)
    os.write(descriptor, b"fixture-data")
    os.close(descriptor)
''')
        extractor.chmod(0o755)
        image = self.root / "fixture.AppImage"
        image.write_bytes(b"type2-runtime-prefixhsqs")
        destination = self.root / "extracted"
        previous = os.umask(0o077)
        try:
            with mock.patch.dict(os.environ, {"PATH": f"{tools}{os.pathsep}{os.environ.get('PATH', '')}"}), mock.patch.object(packaging, "run", NATIVE_RUN):
                packaging.extract_appimage(image, destination)
            self.assertEqual(os.umask(0o077), 0o077)
            self.assertEqual((destination / "AppRun").stat().st_mode & 0o777, 0o755)
            self.assertEqual((destination / "unsafe-executable").stat().st_mode & 0o777, 0o777)
            self.assertEqual((destination / "unsafe-data").stat().st_mode & 0o777, 0o666)
            appdir = self.stage()
            for name in ("unsafe-executable", "unsafe-data"):
                target = appdir / name
                shutil.copy2(destination / name, target)
                with self.assertRaisesRegex(packaging.PackagingError, "public package file permissions"):
                    packaging.audit(appdir)
                target.unlink()
        finally:
            os.umask(previous)


if __name__ == "__main__":
    unittest.main()
