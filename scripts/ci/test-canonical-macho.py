#!/usr/bin/env python3
import importlib.util
import struct
import subprocess
import tempfile
import unittest
from pathlib import Path


CANONICALIZER_PATH = Path(__file__).with_name("canonical-ios-app-hash.py")
CANONICALIZER_SPEC = importlib.util.spec_from_file_location(
    "canonical_ios_app_hash", CANONICALIZER_PATH
)
if CANONICALIZER_SPEC is None or CANONICALIZER_SPEC.loader is None:
    raise RuntimeError(f"could not load {CANONICALIZER_PATH}")
CANONICALIZER = importlib.util.module_from_spec(CANONICALIZER_SPEC)
CANONICALIZER_SPEC.loader.exec_module(CANONICALIZER)

MH_MAGIC_64 = 0xFEEDFACF
CPU_TYPE_X86_64 = 0x01000007
MH_EXECUTE = 0x2
LC_SYMTAB = 0x2
N_STAB = 0xE0
N_TYPE = 0x0E
N_EXT = 0x01
N_ABS = 0x02


def local_absolute_symbols(path):
    data = Path(path).read_bytes()
    if len(data) < 32:
        raise AssertionError(f"Mach-O header is truncated: {path}")

    magic, cpu_type, _, file_type, ncmds, _, _, _ = struct.unpack_from(
        "<IiiIIIII", data
    )
    if magic != MH_MAGIC_64 or cpu_type != CPU_TYPE_X86_64 or file_type != MH_EXECUTE:
        raise AssertionError(f"not an x86_64 Mach-O executable: {path}")

    command_offset = 32
    symtab = None
    for _ in range(ncmds):
        command, command_size = struct.unpack_from("<II", data, command_offset)
        if command == LC_SYMTAB:
            symtab = struct.unpack_from("<IIII", data, command_offset + 8)
            break
        command_offset += command_size

    if symtab is None:
        raise AssertionError(f"Mach-O has no symbol table: {path}")

    symbol_offset, symbol_count, string_offset, string_size = symtab
    string_end = string_offset + string_size
    symbols = []
    for index in range(symbol_count):
        entry_offset = symbol_offset + index * 16
        string_index, symbol_type, _, _, value = struct.unpack_from(
            "<IBBHQ", data, entry_offset
        )
        if symbol_type & N_STAB:
            continue
        if (symbol_type & N_TYPE) != N_ABS or (symbol_type & N_EXT):
            continue

        name_offset = string_offset + string_index
        name_end = data.find(b"\0", name_offset, string_end)
        if name_end == -1:
            raise AssertionError(f"unterminated symbol name in {path}")
        symbols.append((data[name_offset:name_end].decode(), value))

    return symbols


def swap_local_absolute_symbols(path):
    """Swap the first two local absolute nlist entries in place."""
    data = bytearray(Path(path).read_bytes())
    _, _, _, _, ncmds, _, _, _ = struct.unpack_from("<IiiIIIII", data)
    command_offset = 32
    for _ in range(ncmds):
        command, command_size = struct.unpack_from("<II", data, command_offset)
        if command == LC_SYMTAB:
            symbol_offset, symbol_count, _, _ = struct.unpack_from(
                "<IIII", data, command_offset + 8
            )
            break
        command_offset += command_size
    else:
        raise AssertionError(f"Mach-O has no symbol table: {path}")
    entries = []
    for index in range(symbol_count):
        entry_offset = symbol_offset + index * 16
        symbol_type = data[entry_offset + 4]
        if not symbol_type & N_STAB and (symbol_type & N_TYPE) == N_ABS and not symbol_type & N_EXT:
            entries.append(entry_offset)
    first, second = entries[:2]
    data[first:first + 16], data[second:second + 16] = (
        data[second:second + 16],
        data[first:first + 16],
    )
    Path(path).write_bytes(bytes(data))


class CanonicalMachoTests(unittest.TestCase):
    def build_executable(self, output, local_symbols, return_value, global_symbol):
        object_path = output.with_suffix(".o")
        assembly = "\n".join(
            [
                ".text",
                ".globl _main",
                "_main:",
                f"  movl ${return_value}, %eax",
                "  retq",
                f".globl {global_symbol}",
                f".set {global_symbol}, 41",
                *(f".set {name}, {value}" for name, value in local_symbols),
                "",
            ]
        )

        subprocess.run(
            [
                CANONICALIZER.developer_tool("clang"),
                "-target",
                "x86_64-apple-macos13.3",
                "-Wa,-L",
                "-x",
                "assembler",
                "-c",
                "-o",
                str(object_path),
                "-",
            ],
            input=assembly,
            text=True,
            check=True,
        )
        subprocess.run(
            [
                CANONICALIZER.developer_tool("ld"),
                "-static",
                "-arch",
                "x86_64",
                "-e",
                "_main",
                "-o",
                str(output),
                str(object_path),
            ],
            check=True,
        )

    def test_local_absolute_symbol_order_is_not_runtime_content(self):
        symbols_ab = [(".Lalpha", 17), (".Lbeta", 29)]
        symbols_ba = list(reversed(symbols_ab))

        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)
            executable_ab = temp_path / "symbols-ab"
            executable_ba = temp_path / "symbols-ba"
            runtime_change = temp_path / "runtime-change"
            global_change = temp_path / "global-change"

            self.build_executable(
                executable_ab, symbols_ab, return_value=0, global_symbol="_global_alpha"
            )
            self.build_executable(
                executable_ba, symbols_ba, return_value=0, global_symbol="_global_alpha"
            )
            self.build_executable(
                runtime_change, symbols_ab, return_value=1, global_symbol="_global_alpha"
            )
            self.build_executable(
                global_change, symbols_ab, return_value=0, global_symbol="_global_bravo"
            )

            self.assertEqual(local_absolute_symbols(executable_ab), symbols_ab)
            if local_absolute_symbols(executable_ba) == symbols_ab:
                # Xcode 27's ld sorts local absolute symbols, so reproduce the
                # input-order variant older linkers emitted by swapping the entries.
                swap_local_absolute_symbols(executable_ba)
            self.assertEqual(local_absolute_symbols(executable_ba), symbols_ba)

            canonical_ab = CANONICALIZER.canonical_macho(executable_ab)
            canonical_ba = CANONICALIZER.canonical_macho(executable_ba)
            canonical_runtime_change = CANONICALIZER.canonical_macho(runtime_change)
            canonical_global_change = CANONICALIZER.canonical_macho(global_change)

            self.assertEqual(canonical_ab, canonical_ba)
            self.assertNotEqual(canonical_ab, canonical_runtime_change)
            self.assertNotEqual(canonical_ab, canonical_global_change)


if __name__ == "__main__":
    unittest.main()
