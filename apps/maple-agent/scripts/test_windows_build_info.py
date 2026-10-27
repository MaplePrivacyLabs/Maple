#!/usr/bin/env python3
"""Hermetic PE section, DLL closure and non-executing identity fixtures."""

import importlib.util
import json
import os
from pathlib import Path
import shutil
import struct
import subprocess
import sys
import tempfile
import unittest
from unittest import mock


SCRIPTS = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("windows_build_info", SCRIPTS / "windows-build-info.py")
metadata = importlib.util.module_from_spec(spec)
spec.loader.exec_module(metadata)
spec = importlib.util.spec_from_file_location("release_info", SCRIPTS / "release-info.py")
release_info = importlib.util.module_from_spec(spec)
spec.loader.exec_module(release_info)
SOURCE_SHA = "a" * 40
PE_OFFSET = 0x80
OPTIONAL = PE_OFFSET + 24
SECTIONS = OPTIONAL + 240


def public_info(profile="dev", source_sha=SOURCE_SHA):
    profiles = json.loads((SCRIPTS.parent / "release-profiles.json").read_text())
    public = {field: profiles[profile][field] for field in release_info.PUBLIC_FIELDS}
    return dict(public, update_feed_url=profiles[profile]["windows_update_feed_url"], profile=profile, version="0.1.0",
                git_revision=source_sha[:8], source_sha=source_sha)


def aligned(value, alignment):
    return (value + alignment - 1) // alignment * alignment


def pe(payload, *, machine=metadata.MACHINE_AMD64, characteristics=0x22, duplicate=False,
       imports=(), delay_imports=()):
    """Create a PE32+ image fixture using no native Windows tools."""
    if not isinstance(payload, bytes):
        payload = json.dumps(payload, sort_keys=True, separators=(",", ":")).encode()
    sections = [(b".text", b"\xc3", 0x60000020), (b".maple", payload, 0x40000040)]
    if duplicate:
        sections.append((b".maple", payload, 0x40000040))
    count = len(sections) + bool(imports or delay_imports)
    headers_size = aligned(SECTIONS + 40 * count, 512)
    headers = bytearray(headers_size)
    headers[:2] = b"MZ"
    struct.pack_into("<I", headers, 0x3C, PE_OFFSET)
    headers[PE_OFFSET:PE_OFFSET + 24] = b"PE\0\0" + struct.pack("<HHIIIHH", machine, count, 0, 0, 0, 240, characteristics)
    struct.pack_into("<H", headers, OPTIONAL, 0x20B)
    struct.pack_into("<I", headers, OPTIONAL + 16, 0x1000)
    struct.pack_into("<Q", headers, OPTIONAL + 24, 0x140000000)
    struct.pack_into("<II", headers, OPTIONAL + 32, 4096, 512)
    struct.pack_into("<I", headers, OPTIONAL + 60, headers_size)
    struct.pack_into("<H", headers, OPTIONAL + 68, 3)
    struct.pack_into("<I", headers, OPTIONAL + 108, 16)
    address = 0x1000
    body = bytearray()
    for index, (name, content, flags) in enumerate(sections):
        raw_size = aligned(len(content), 512)
        struct.pack_into("<8sIIIIIIHHI", headers, SECTIONS + 40 * index, name,
                         len(content), address, raw_size, headers_size + len(body), 0, 0, 0, 0, flags)
        body += content + bytes(raw_size - len(content))
        address += aligned(max(1, len(content)), 4096)
    if imports or delay_imports:
        regular_size = 20 * (len(imports) + 1) if imports else 0
        delay_size = 32 * (len(delay_imports) + 1) if delay_imports else 0
        content = bytearray(regular_size + delay_size)
        for index, name in enumerate(imports):
            name_address = address + len(content)
            content += name.encode() + b"\0"
            content += bytes(aligned(len(content), 8) - len(content))
            thunk_address = address + len(content)
            content += bytes(8)
            struct.pack_into("<I", content, index * 20, thunk_address)
            struct.pack_into("<I", content, index * 20 + 12, name_address)
            struct.pack_into("<I", content, index * 20 + 16, thunk_address)
        for index, name in enumerate(delay_imports):
            name_address = address + len(content)
            content += name.encode() + b"\0"
            struct.pack_into("<II", content, regular_size + index * 32, 1, name_address)
        if imports:
            struct.pack_into("<II", headers, OPTIONAL + 112 + 8, address, regular_size)
        if delay_imports:
            struct.pack_into("<II", headers, OPTIONAL + 112 + 13 * 8, address + regular_size, delay_size)
        raw_size = aligned(len(content), 512)
        struct.pack_into("<8sIIIIIIHHI", headers, SECTIONS + 40 * (count - 1), b".idata",
                         len(content), address, raw_size, headers_size + len(body), 0, 0, 0, 0, 0x40000040)
        body += content + bytes(raw_size - len(content))
        address += aligned(len(content), 4096)
    struct.pack_into("<I", headers, OPTIONAL + 56, address)
    return bytes(headers + body)


class StaticWindowsMetadataTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.binary = Path(temporary.name) / "maple-agent.exe"

    def write(self, payload=None, **options):
        self.binary.write_bytes(pe(public_info() if payload is None else payload, **options))

    def mutate(self, offset, value, kind="<I"):
        data = bytearray(self.binary.read_bytes())
        struct.pack_into(kind, data, offset, value)
        self.binary.write_bytes(data)

    def test_exact_public_identity_without_execution_and_profile_binding(self):
        self.write()
        with mock.patch.object(subprocess, "run", side_effect=AssertionError("artifact executed")), \
                mock.patch.object(subprocess, "Popen", side_effect=AssertionError("artifact executed")):
            self.assertEqual(metadata.validated_build_info("dev", self.binary, SOURCE_SHA), public_info())
        with self.assertRaisesRegex(ValueError, "profile"):
            metadata.validated_build_info("prod", self.binary, SOURCE_SHA)
        with self.assertRaisesRegex(ValueError, "source commit"):
            metadata.validated_build_info("dev", self.binary, "b" * 40)
        for source in (None, "", "a" * 8):
            with self.subTest(source=source), self.assertRaisesRegex(ValueError, "full lowercase SHA"):
                metadata.validated_build_info("dev", self.binary, source)

    def test_both_profiles_use_their_own_windows_update_feed(self):
        for profile in ("dev", "prod"):
            self.write(public_info(profile))
            self.assertEqual(metadata.validated_build_info(profile, self.binary, SOURCE_SHA), public_info(profile))
            self.assertEqual(public_info(profile)["update_feed_url"],
                             "https://updates.maple.ai/agent/stable/windows/latest.json" if profile == "prod" else None)
        for url in ("https://updates.maple.ai/agent/stable/latest.json",
                    "https://updates.maple.ai/agent/stable/linux/latest.json", None):
            self.write({**public_info("prod"), "update_feed_url": url})
            with self.subTest(url=url), self.assertRaisesRegex(ValueError, "update_feed_url"):
                metadata.validated_build_info("prod", self.binary, SOURCE_SHA)

    def test_rejects_non_pe_architectures_dlls_and_bad_optional_headers(self):
        for options in ({"machine": 0x14C}, {"machine": 0xAA64}, {"characteristics": 0x2022}, {"characteristics": 0}):
            self.write(**options)
            with self.subTest(options=options), self.assertRaisesRegex(ValueError, "x64 PE executable"):
                metadata.read_build_info(self.binary)
        for offset, value, kind in ((0, 0, "<H"), (PE_OFFSET, 0, "<I"), (0x3C, 63, "<I"),
                                    (0x3C, metadata.MAX_HEADER_BYTES + 1, "<I"),
                                    (OPTIONAL, 0x10B, "<H"), (OPTIONAL + 68, 10, "<H")):
            self.write()
            self.mutate(offset, value, kind)
            with self.subTest(offset=offset), self.assertRaises(ValueError):
                metadata.read_build_info(self.binary)

    def test_rejects_oversized_tables_out_of_bounds_and_overlapping_sections(self):
        for offset, value, kind in ((PE_OFFSET + 6, 97, "<H"), (PE_OFFSET + 20, 4097, "<H"),
                                    (OPTIONAL + 108, 17, "<I"), (OPTIONAL + 60, 1, "<I"),
                                    (SECTIONS + 40 + 12, 0x1000, "<I"),
                                    (SECTIONS + 40 + 20, 512, "<I"),
                                    (SECTIONS + 40 + 20, 0xFFFFFFFF, "<I"),
                                    (SECTIONS + 40 + 24, 1, "<I")):
            self.write()
            self.mutate(offset, value, kind)
            with self.subTest(offset=offset, value=value), self.assertRaises(ValueError):
                metadata.read_build_info(self.binary)
        baseline = pe(public_info())
        for length in (0, 1, 63, PE_OFFSET + 23, OPTIONAL + 12, len(baseline) - 1):
            self.binary.write_bytes(baseline[:length])
            with self.subTest(length=length), self.assertRaises(ValueError):
                metadata.read_build_info(self.binary)

    def test_requires_one_readonly_initialized_metadata_section(self):
        self.write(duplicate=True)
        with self.assertRaisesRegex(ValueError, "exactly one"):
            metadata.read_build_info(self.binary)
        for flags in (0x40, 0x40000000, 0xC0000040, 0x60000040):
            self.write()
            self.mutate(SECTIONS + 40 + 36, flags)
            with self.subTest(flags=flags), self.assertRaisesRegex(ValueError, "read-only"):
                metadata.read_build_info(self.binary)
        self.binary.write_bytes(pe(public_info()).replace(b".maple\0\0", b".other\0\0"))
        with self.assertRaisesRegex(ValueError, "exactly one"):
            metadata.read_build_info(self.binary)

    def test_rejects_invalid_json_duplicates_padding_and_oversized_metadata(self):
        for payload in (b'{"profile":"dev","profile":"prod"}', b"\xff", b"{}\0", b"{}{}", b"x" * (metadata.MAX_METADATA_BYTES + 1)):
            self.write(payload)
            with self.subTest(payload=payload[:30]), self.assertRaises(ValueError):
                metadata.read_build_info(self.binary)
        self.write()
        content = bytearray(self.binary.read_bytes())
        content[-1] = 1
        self.binary.write_bytes(content)
        with self.assertRaisesRegex(ValueError, "padding"):
            metadata.read_build_info(self.binary)

    def test_rejects_corrupt_profile_metadata(self):
        for field, value in (("unexpected", "field"), ("api_url", "https://wrong.example"),
                             ("source_sha", "short"), ("git_revision", "aaaaaaaa-dirty"),
                             ("update_feed_url", "https://updates.maple.ai/agent/stable/latest.json")):
            self.write(dict(public_info(), **{field: value}))
            with self.subTest(field=field), self.assertRaises(ValueError):
                metadata.validated_build_info("dev", self.binary, SOURCE_SHA)

    def test_imports_include_lowercase_regular_and_delay_loaded_dlls(self):
        self.write(imports=("KERNEL32.dll", "USER32.DLL", "WINSPOOL.DRV"), delay_imports=("D3D11.dll", "KERNEL32.dll"))
        self.assertEqual(metadata.inspect_pe(self.binary), {
            "machine": "x86_64", "subsystem": 3,
            "imports": ["d3d11.dll", "kernel32.dll", "user32.dll", "winspool.drv"],
        })
        self.write()
        self.assertEqual(metadata.inspect_pe(self.binary)["imports"], [])

    def test_import_names_cannot_hide_paths_or_unterminated_oversized_strings(self):
        for name in ("../kernel32.dll", "C:\\kernel32.dll", "dir/kernel32.dll", "bad:name.dll", "x" * 256 + ".dll", "kernel32.exe"):
            self.write(imports=(name,))
            with self.subTest(name=name), self.assertRaisesRegex(ValueError, "DLL name"):
                metadata.inspect_pe(self.binary)

    def test_rejects_bad_import_rvas_missing_terminators_and_oversized_tables(self):
        for directory, record_size in ((1, 20), (13, 32)):
            for field, value in ((0, 1), (4, 1), (4, metadata.MAX_IMPORT_BYTES + 1), (4, record_size)):
                self.write(imports=("kernel32.dll",), delay_imports=("d3d11.dll",))
                self.mutate(OPTIONAL + 112 + directory * 8 + field, value)
                with self.subTest(directory=directory, field=field, value=value), self.assertRaises(ValueError):
                    metadata.inspect_pe(self.binary)
        self.write(imports=("kernel32.dll",), delay_imports=("d3d11.dll",))
        raw = struct.unpack_from("<I", self.binary.read_bytes(), SECTIONS + 80 + 20)[0]
        for offset, value in ((raw + 12, 0), (raw + 40, 0)):
            self.write(imports=("kernel32.dll",), delay_imports=("d3d11.dll",))
            self.mutate(offset, value)
            with self.subTest(offset=offset), self.assertRaises(ValueError):
                metadata.inspect_pe(self.binary)

    @unittest.skipIf(sys.platform == "win32", "Windows symlink creation requires an OS privilege")
    def test_rejects_symlinks(self):
        original = self.binary.with_name("original.exe")
        original.write_bytes(pe(public_info()))
        self.binary.symlink_to(original)
        with self.assertRaisesRegex(ValueError, "regular file"):
            metadata.read_build_info(self.binary)

    @unittest.skipUnless(hasattr(os, "mkfifo"), "FIFO fixture requires Unix")
    def test_rejects_fifo_before_reading(self):
        os.mkfifo(self.binary)
        with self.assertRaisesRegex(ValueError, "regular file"):
            metadata.read_build_info(self.binary)

    def test_cli_emits_canonical_identity_only(self):
        self.write()
        result = subprocess.run([sys.executable, str(SCRIPTS / "windows-build-info.py"),
                                 str(self.binary), "--profile", "dev", "--source-sha", SOURCE_SHA],
                                check=True, capture_output=True, text=True)
        self.assertEqual(result.stdout, json.dumps(public_info(), sort_keys=True, separators=(",", ":")) + "\n")
        self.assertEqual(result.stderr, "")

    @unittest.skipUnless(sys.platform == "win32" and shutil.which("rustc"), "native PE fixture requires Windows and Rust")
    def test_native_rust_metadata_survives_release_lto_and_symbol_stripping(self):
        root = self.binary.parent
        info = public_info()
        (root / "build-info.json").write_text(json.dumps(info, sort_keys=True, separators=(",", ":")))
        source = root / "fixture.rs"
        source.write_text('''#[used]
#[unsafe(no_mangle)]
#[unsafe(link_section = ".maple")]
static MAPLE_AGENT_BUILD_INFO: [u8; include_bytes!("build-info.json").len()] = *include_bytes!("build-info.json");
fn main() { panic!("the signing preflight must never execute an artifact"); }
''')
        command = ["rustc", "--edition=2024", "-C", "opt-level=3", "-C", "lto=fat", "-C", "strip=symbols",
                   "-C", "link-arg=/INCLUDE:MAPLE_AGENT_BUILD_INFO"]
        # Direct rustc does not consume Cargo's target-linker environment.
        # Preserve the native linker already selected and smoked by CI, rather
        # than letting Git Bash's unrelated link.exe win PATH resolution.
        linker = os.environ.get("CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER")
        if os.environ.get("GITHUB_ACTIONS") == "true":
            self.assertIsNotNone(linker, "native fixture requires CI's verified MSVC linker")
        if linker:
            selected = Path(linker)
            self.assertTrue(selected.is_absolute() and selected.is_file() and selected.name.lower() == "link.exe",
                            "native fixture linker must be the verified absolute MSVC link.exe")
            command.extend(["-C", f"linker={selected}"])
        subprocess.run([*command, str(source), "-o", str(self.binary)],
                       check=True, capture_output=True, text=True)
        self.assertEqual(metadata.validated_build_info("dev", self.binary, SOURCE_SHA), info)
        self.assertIn("kernel32.dll", metadata.inspect_pe(self.binary)["imports"])


if __name__ == "__main__":
    unittest.main()
