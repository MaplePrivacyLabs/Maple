#!/usr/bin/env python3
"""Linux packages link against and are verified at the supported glibc floor."""

import os
from pathlib import Path
import shutil
import struct
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
HELPER = ROOT / "scripts/ci/elf_glibc_floor.py"

SHT_STRTAB = 3
SHT_DYNSYM = 11
SHT_GNU_VERNEED = 0x6FFFFFFE
SHT_GNU_VERSYM = 0x6FFFFFFF


def synthetic_elf(needed):
    """An ELF64 shared-object skeleton whose undefined symbols need the given versions.

    needed: [(library, version name, [symbol, ...]), ...]
    """
    strings = bytearray(b"\0")
    offsets = {}

    def intern(text):
        if text not in offsets:
            offsets[text] = len(strings)
            strings.extend(text.encode() + b"\0")
        return offsets[text]

    dynsym = bytearray(struct.pack("<IBBHQQ", 0, 0, 0, 0, 0, 0))
    versym = bytearray(struct.pack("<H", 0))
    verneed = bytearray()
    version_index = 2
    for library_position, (library, version, symbols) in enumerate(needed):
        for symbol in symbols:
            dynsym += struct.pack("<IBBHQQ", intern(symbol), 0x12, 0, 0, 0, 0)
            versym += struct.pack("<H", version_index)
        last = library_position == len(needed) - 1
        verneed += struct.pack("<HHIII", 1, 1, intern(library), 16, 0 if last else 32)
        verneed += struct.pack("<IHHII", 0, 0, version_index, intern(version), 0)
        version_index += 1

    names = bytearray(b"\0")
    name_offsets = {}
    for name in (".dynstr", ".dynsym", ".gnu.version", ".gnu.version_r", ".shstrtab"):
        name_offsets[name] = len(names)
        names.extend(name.encode() + b"\0")

    blobs = [bytes(strings), bytes(dynsym), bytes(versym), bytes(verneed), bytes(names)]
    body = bytearray()
    data_offsets = []
    position = 64
    for blob in blobs:
        data_offsets.append(position)
        body += blob
        position += len(blob)
    while position % 8:
        body += b"\0"
        position += 1
    shoff = position

    def header(name, kind, offset, size, link=0, info=0, entsize=0):
        return struct.pack("<IIQQQQIIQQ", name, kind, 0, 0, offset, size, link, info, 1, entsize)

    headers = header(0, 0, 0, 0)
    headers += header(name_offsets[".dynstr"], SHT_STRTAB, data_offsets[0], len(blobs[0]))
    headers += header(name_offsets[".dynsym"], SHT_DYNSYM, data_offsets[1], len(blobs[1]), link=1, info=1, entsize=24)
    headers += header(name_offsets[".gnu.version"], SHT_GNU_VERSYM, data_offsets[2], len(blobs[2]), link=2, entsize=2)
    headers += header(name_offsets[".gnu.version_r"], SHT_GNU_VERNEED, data_offsets[3], len(blobs[3]), link=1,
                      info=len(needed))
    headers += header(name_offsets[".shstrtab"], SHT_STRTAB, data_offsets[4], len(blobs[4]))

    ident = b"\x7fELF" + bytes([2, 1, 1, 0]) + bytes(8)
    elf_header = ident + struct.pack("<HHIQQQIHHHHHH", 3, 62, 1, 0, 0, shoff, 0, 64, 0, 0, 64, 6, 5)
    assert len(elf_header) == 64
    return bytes(elf_header) + bytes(body) + headers


class ElfGlibcFloorTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.directory = Path(directory.name)

    def write(self, name, needed):
        path = self.directory / name
        path.write_bytes(synthetic_elf(needed))
        return path

    def run_helper(self, *arguments):
        return subprocess.run([sys.executable, "-I", str(HELPER), *arguments],
                              capture_output=True, text=True, check=False)

    def test_versions_at_or_below_the_floor_pass_and_report_the_newest(self):
        path = self.write("ok", [("libc.so.6", "GLIBC_2.34", ["dlopen", "pthread_create"]),
                                 ("libm.so.6", "GLIBC_2.35", ["hypotf"]),
                                 ("libc.so.6", "GLIBC_2.39", ["pidfd_spawnp"]),
                                 ("libgtk-3.so.0", "GTK_3_0", ["gtk_init"])])
        result = self.run_helper("--floor", "2.39", str(path))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, f"verified-linux-glibc-floor  GLIBC_2.39  floor=2.39  {path}\n")

    def test_a_newer_version_fails_and_names_its_symbols(self):
        path = self.write("new", [("libc.so.6", "GLIBC_2.34", ["dlopen"]),
                                  ("libm.so.6", "GLIBC_2.43", ["acosf", "atan2f"]),
                                  ("libc.so.6", "GLIBC_2.40", ["something_new"])])
        result = self.run_helper("--floor", "2.39", str(path))
        self.assertEqual(result.returncode, 1)
        self.assertEqual(result.stdout, "")
        self.assertIn("above the 2.39 floor", result.stderr)
        self.assertIn("libm.so.6 GLIBC_2.43: acosf, atan2f", result.stderr)
        self.assertIn("libc.so.6 GLIBC_2.40: something_new", result.stderr)
        self.assertNotIn("GLIBC_2.34", result.stderr)
        # The same file passes once the floor covers it, so the comparison is numeric.
        self.assertEqual(self.run_helper("--floor", "2.43", str(path)).returncode, 0)
        self.assertEqual(self.run_helper("--floor", "2.4", str(path)).returncode, 1)

    def test_one_failing_file_fails_the_whole_run(self):
        good = self.write("good", [("libc.so.6", "GLIBC_2.34", ["dlopen"])])
        bad = self.write("bad", [("libc.so.6", "GLIBC_2.41", ["x"])])
        result = self.run_helper("--floor", "2.39", str(good), str(bad))
        self.assertEqual(result.returncode, 1)
        self.assertIn(f"verified-linux-glibc-floor  GLIBC_2.34  floor=2.39  {good}", result.stdout)
        self.assertIn(f"{bad} needs glibc", result.stderr)

    def test_files_without_glibc_version_needs_pass(self):
        path = self.write("static", [])
        result = self.run_helper("--floor", "2.39", str(path))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("GLIBC_none", result.stdout)

    def test_loader_abi_markers_count_by_the_release_that_defines_them(self):
        # Every glibc from 2.36 defines DT_RELR; the floor's glibc lacks the TLS-descriptor marker.
        relr = self.write("relr", [("libc.so.6", "GLIBC_2.34", ["dlopen"]), ("libc.so.6", "GLIBC_ABI_DT_RELR", [])])
        result = self.run_helper("--floor", "2.39", str(relr))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, f"verified-linux-glibc-floor  GLIBC_2.34  floor=2.39  {relr}\n")
        self.assertEqual(self.run_helper("--floor", "2.35", str(relr)).returncode, 1)

        for marker in ("GLIBC_ABI_GNU2_TLS", "GLIBC_ABI_SOMETHING_NEW"):
            with self.subTest(marker=marker):
                path = self.write("marker", [("libc.so.6", "GLIBC_2.34", ["dlopen"]), ("libc.so.6", marker, [])])
                result = self.run_helper("--floor", "2.39", str(path))
                self.assertEqual(result.returncode, 1)
                self.assertIn(f"libc.so.6 {marker}: loader ABI marker", result.stderr)
                self.assertNotIn("invalid glibc version", result.stderr)
                # No numbered release covers a marker the floor does not define.
                self.assertEqual(self.run_helper("--floor", "2.99", str(path)).returncode, 1)

    def test_a_fortified_glibc_import_is_required_only_on_request(self):
        fortified = self.write("fortified", [("libc.so.6", "GLIBC_2.34", ["dlopen"]),
                                             ("libc.so.6", "GLIBC_2.3.4", ["__vfprintf_chk", "__vsnprintf_chk"])])
        result = self.run_helper("--floor", "2.39", "--require-fortify", str(fortified))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, f"verified-linux-glibc-floor  GLIBC_2.34  floor=2.39  fortify=2  {fortified}\n")

        # zig's compiler-rt defines __stack_chk_fail itself, so that import is no evidence either way.
        for needed in ([("libc.so.6", "GLIBC_2.34", ["dlopen"])],
                       [("libc.so.6", "GLIBC_2.4", ["__stack_chk_fail"])]):
            with self.subTest(needed=needed):
                path = self.write("soft", needed)
                self.assertEqual(self.run_helper("--floor", "2.39", str(path)).returncode, 0)
                result = self.run_helper("--floor", "2.39", "--require-fortify", str(path))
                self.assertEqual(result.returncode, 1)
                self.assertEqual(result.stdout, "")
                self.assertIn("imports no fortified glibc __*_chk function", result.stderr)

    def test_non_elf_input_fails_closed(self):
        path = self.directory / "text"
        path.write_bytes(b"#!/bin/sh\nexec maple\n")
        result = self.run_helper("--floor", "2.39", str(path))
        self.assertEqual(result.returncode, 1)
        self.assertIn("not an ELF file", result.stderr)
        self.assertEqual(self.run_helper("--floor", "2.39", str(self.directory / "missing")).returncode, 1)


class LinuxGlibcFloorLinkerTests(unittest.TestCase):
    """configure_linux_glibc_floor_linker points Cargo at zig's glibc floor."""

    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.log = self.root / "zigbuild.log"
        self.fake("cargo-zigbuild", f'printf "%s\\n" "$@" > "{self.log}"\n')
        self.fake("zig", 'printf "0.17.0\\n"\n')
        self.fake("rustc", 'printf "rustc 1.94.1\\nhost: %s\\nrelease: 1.94.1\\n" "${FAKE_RUST_HOST:-x86_64-unknown-linux-gnu}"\n')
        closure = self.root / "closure"
        closure.mkdir()
        for store in ("gtk3", "webkitgtk"):
            (self.root / store / "lib").mkdir(parents=True)
        (closure / "store-paths").write_text(f"{self.root / 'gtk3'}\n{self.root / 'webkitgtk'}\n{self.root / 'no-lib'}\n")
        self.closure = closure

    def fake(self, name, body):
        # The Nix check sandbox has no /usr/bin/env; name the bash on PATH.
        path = self.bin / name
        path.write_text(f"#!{shutil.which('bash')}\n" + body)
        path.chmod(0o755)

    def run_shell(self, commands, **extra_env):
        for name in ("CFLAGS", "CXXFLAGS", "CMAKE_C_FLAGS", "CMAKE_CXX_FLAGS"):
            extra_env.setdefault(name, "")
        env = dict(os.environ, PATH=f"{self.bin}:{os.environ['PATH']}",
                   MAPLE_NIX_LINUX_CLOSURE_INFO=str(self.closure), FIXTURE_ROOT=str(self.root),
                   MAPLE_NIX_ELF_INTERPRETER="/nix/store/fixture-glibc/lib/ld-linux-x86-64.so.2", **extra_env)
        env.pop("MAPLE_NIX_GCC_LIB", None)
        env.pop("RUSTFLAGS", None)
        env.pop("CARGO_TARGET_DIR", None)
        return subprocess.run(
            ["bash", "-c", "source scripts/ci/_common.sh\n"
             'TAURI_DIR="$FIXTURE_ROOT/tauri"\n'
             "host_os() { echo linux; }\n" + commands],
            cwd=ROOT, env=env, capture_output=True, text=True)

    def test_cargo_links_and_compiles_c_through_the_floor_wrappers(self):
        result = self.run_shell(
            "configure_linux_glibc_floor_linker\n"
            'printf "linker=%s\\ncc=%s\\ncxx=%s\\nrustflags=%s\\n" '
            '"$CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER" "$CC_x86_64_unknown_linux_gnu" '
            '"$CXX_x86_64_unknown_linux_gnu" "$RUSTFLAGS"\n'
            '"$CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER" -o out a.o "-Wl,--as-needed"\n')
        self.assertEqual(result.returncode, 0, result.stderr)
        values = dict(line.split("=", 1) for line in result.stdout.splitlines() if "=" in line)
        wrapper_dir = self.root / "tauri/target/glibc-floor"
        self.assertEqual(values["linker"], str(wrapper_dir / "zig-cc"))
        self.assertEqual(values["cc"], str(wrapper_dir / "zig-cc"))
        self.assertEqual(values["cxx"], str(wrapper_dir / "zig-c++"))
        self.assertIn("configured-linux-glibc-floor  2.39  x86_64-linux-gnu.2.39", result.stdout)
        # The wrapper hands rustc's and the cc crate's arguments to cargo-zigbuild's zig cc at the floor.
        self.assertEqual(self.log.read_text().splitlines(),
                         ["zig", "cc", "--", "-target", "x86_64-linux-gnu.2.39", "-o", "out", "a.o", "-Wl,--as-needed"])
        cxx = (wrapper_dir / "zig-c++").read_text()
        self.assertIn("zig c++ -- -target x86_64-linux-gnu.2.39", cxx)
        # The wrapper runs on the shell's own bash, not the host's /bin/sh, which the
        # build's Nix LD_LIBRARY_PATH would break.
        self.assertEqual(cxx.splitlines()[0], "#!" + shutil.which("bash", path=f"{self.bin}:{os.environ['PATH']}"))
        # The wrapper names the resolved zig and rustc, so cargo-zigbuild never probes PATH,
        # python or `$RUSTC_WRAPPER rustc`, which fails inside sccache's server.
        self.assertIn(f'CARGO_ZIGBUILD_ZIG_COMMAND="{self.bin / "zig"}"', cxx)
        self.assertIn('CARGO_ZIGBUILD_ZIG_VERSION="0.17.0"', cxx)
        self.assertIn('CARGO_ZIGBUILD_RUSTC_VERSION="1.94.1"', cxx)
        # Executables get the Nix loader while they build and run under the Nix LD_LIBRARY_PATH,
        # and Nix libraries are link-time stand-ins for the host's; their own glibc symbols are not errors.
        flags = values["rustflags"].split()
        self.assertEqual(flags[:4], ["-C", "link-arg=-Wl,--dynamic-linker=/nix/store/fixture-glibc/lib/ld-linux-x86-64.so.2",
                                     "-C", "link-arg=-Wl,--allow-shlib-undefined"])
        searched = [flags[i + 1] for i, flag in enumerate(flags) if flag == "-L"]
        self.assertEqual(searched, [f"native={self.root / 'gtk3/lib'}", f"native={self.root / 'webkitgtk/lib'}"])

    def test_c_dependencies_keep_the_nix_wrapper_hardening(self):
        # zig cc ignores NIX_HARDENING_ENABLE, so the flags travel in the variables the cc crate and CMake read.
        result = self.run_shell(
            'export CFLAGS="-ffile-prefix-map=/src=/maple"\n'
            "configure_linux_glibc_floor_linker >/dev/null\n"
            "configure_linux_glibc_floor_linker >/dev/null\n"
            'printf "%s\n" "$CFLAGS" "$CXXFLAGS" "$CMAKE_C_FLAGS" "$CMAKE_CXX_FLAGS"\n')
        self.assertEqual(result.returncode, 0, result.stderr)
        cflags, cxxflags, cmake_c, cmake_cxx = (line.split() for line in result.stdout.splitlines())
        self.assertEqual(cflags[0], "-ffile-prefix-map=/src=/maple")
        hardening = cflags[1:]
        self.assertEqual(cxxflags, hardening)
        self.assertEqual(cmake_c, hardening)
        self.assertEqual(cmake_cxx, hardening)
        self.assertEqual(len(hardening), len(set(hardening)))
        for flag in ("-fstack-protector-strong", "-fstack-clash-protection", "-D_FORTIFY_SOURCE=3"):
            self.assertIn(flag, hardening)
        # The macro is undefined first so a compiler default cannot clash with it.
        self.assertLess(hardening.index("-U_FORTIFY_SOURCE"), hardening.index("-D_FORTIFY_SOURCE=3"))

    def test_existing_rustflags_and_cargo_target_dir_are_kept(self):
        result = self.run_shell(
            'export RUSTFLAGS="--remap-path-prefix=/src=/build"\n'
            'export CARGO_TARGET_DIR="$FIXTURE_ROOT/elsewhere"\n'
            "configure_linux_glibc_floor_linker\n"
            "configure_linux_glibc_floor_linker >/dev/null\n"
            'printf "%s\\n%s\\n" "$RUSTFLAGS" "$CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER"\n')
        self.assertEqual(result.returncode, 0, result.stderr)
        rustflags, linker = result.stdout.splitlines()[-2:]
        self.assertTrue(rustflags.startswith("--remap-path-prefix=/src=/build -C link-arg=-Wl,--dynamic-linker="))
        self.assertEqual(rustflags.count("--allow-shlib-undefined"), 1)
        self.assertEqual(linker, str(self.root / "elsewhere/glibc-floor/zig-cc"))

    def test_missing_tools_or_unsupported_hosts_fail_closed(self):
        (self.bin / "cargo-zigbuild").unlink()
        result = self.run_shell("configure_linux_glibc_floor_linker")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("cargo-zigbuild is required", result.stderr)
        self.fake("cargo-zigbuild", "exit 0\n")
        result = self.run_shell("configure_linux_glibc_floor_linker", FAKE_RUST_HOST="riscv64gc-unknown-linux-gnu")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Unsupported Rust host target", result.stderr)
        result = self.run_shell("unset MAPLE_NIX_ELF_INTERPRETER\nconfigure_linux_glibc_floor_linker")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("MAPLE_NIX_ELF_INTERPRETER is required", result.stderr)

    def test_other_hosts_are_untouched(self):
        result = self.run_shell("host_os() { echo darwin; }\nconfigure_linux_glibc_floor_linker\n"
                                'printf "[%s]" "${CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER:-}"')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "[]")

    def test_package_verification_uses_the_floor_and_requires_fortified_c(self):
        fortified = [("libc.so.6", "GLIBC_2.3.4", ["__vsnprintf_chk"])]
        path = self.root / "maple"
        path.write_bytes(synthetic_elf(fortified + [("libm.so.6", "GLIBC_2.43", ["acosf"])]))
        result = self.run_shell(f'verify_linux_package_executable_glibc_floor "{path}"')
        self.assertEqual(result.returncode, 1)
        self.assertIn("libm.so.6 GLIBC_2.43: acosf", result.stderr)

        path.write_bytes(synthetic_elf([("libc.so.6", "GLIBC_2.39", ["pidfd_spawnp"])]))
        result = self.run_shell(f'verify_linux_package_executable_glibc_floor "{path}"')
        self.assertEqual(result.returncode, 1)
        self.assertIn("imports no fortified glibc __*_chk function", result.stderr)

        path.write_bytes(synthetic_elf(fortified + [("libc.so.6", "GLIBC_2.39", ["pidfd_spawnp"])]))
        result = self.run_shell(f'verify_linux_package_executable_glibc_floor "{path}"')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("GLIBC_2.39  floor=2.39  fortify=1", result.stdout)

if __name__ == "__main__":
    unittest.main()
