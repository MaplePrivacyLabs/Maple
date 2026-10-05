#!/usr/bin/env python3
"""Bounded non-executing Mach-O metadata parser and release-binding fixtures."""

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


spec = importlib.util.spec_from_file_location("macos_build_info", Path(__file__).with_name("macos-build-info.py"))
metadata = importlib.util.module_from_spec(spec)
spec.loader.exec_module(metadata)
SOURCE_SHA = "a" * 40


def public_info(profile="dev", source_sha=SOURCE_SHA):
    profiles = json.loads(Path(__file__).resolve().parent.parent.joinpath("release-profiles.json").read_text())
    return dict(profiles[profile], profile=profile, version="0.1.0", git_revision=source_sha[:8], source_sha=source_sha)


def macho(payload, *, cpu=metadata.CPU_TYPE_ARM64, subtype=0, kind=metadata.MH_EXECUTE, duplicate=False, owner=b"__TEXT"):
    """Create a thin fixture for static preflight tests without native tools."""
    if not isinstance(payload, bytes):
        payload = json.dumps(payload, sort_keys=True, separators=(",", ":")).encode()
    count = 2 if duplicate else 1
    size = 72 + 80 * count
    offset = 32 + size
    address = 0x100000000
    header = struct.pack("<8I", metadata.MH_MAGIC_64, cpu, subtype, kind, 1, size, 0, 0)
    segment = struct.pack("<2I16s4Q4I", metadata.LC_SEGMENT_64, size, b"__TEXT", address,
                          offset + len(payload), 0, offset + len(payload), 5, 5, count, 0)
    section = struct.pack("<16s16s2Q8I", b"__maple_info", owner, address + offset,
                          len(payload), offset, 0, 0, 0, 0, 0, 0, 0)
    return header + segment + section * count + payload


class StaticMetadataTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.binary = Path(self.temporary.name) / "maple-agent"

    def read(self, payload, **options):
        self.binary.write_bytes(macho(payload, **options))
        return metadata.read_build_info(self.binary)

    def test_exact_public_info_roundtrip_and_binding_without_execution(self):
        info = public_info()
        self.binary.write_bytes(macho(info))
        # A metadata read must never invoke executable code or native tools.
        with mock.patch.object(subprocess, "run", side_effect=AssertionError("artifact executed")), \
                mock.patch.object(subprocess, "Popen", side_effect=AssertionError("artifact executed")):
            self.assertEqual(metadata.validated_build_info("dev", self.binary, SOURCE_SHA), info)
        with self.assertRaisesRegex(ValueError, "profile"):
            metadata.validated_build_info("prod", self.binary, SOURCE_SHA)
        with self.assertRaisesRegex(ValueError, "source commit"):
            metadata.validated_build_info("dev", self.binary, "b" * 40)
        for source in (None, "", "a" * 8):
            with self.assertRaisesRegex(ValueError, "full lowercase SHA"):
                metadata.validated_build_info("dev", self.binary, source)

    def test_both_profiles_validate_the_checked_in_public_contract(self):
        for profile in ("dev", "prod"):
            self.binary.write_bytes(macho(public_info(profile)))
            self.assertEqual(metadata.validated_build_info(profile, self.binary, SOURCE_SHA), public_info(profile))

    def test_rejects_other_architectures_subtypes_and_filetypes(self):
        for options in ({"cpu": 0x01000007}, {"subtype": 2}, {"kind": 6}):
            with self.subTest(options=options), self.assertRaisesRegex(ValueError, "ARM64"):
                self.read(public_info(), **options)
        for magic in (0xCAFEBABE, 0xFEEDFACE, 0xCFFAEDFE):
            self.binary.write_bytes(struct.pack("<I", magic) + macho(public_info())[4:])
            with self.assertRaisesRegex(ValueError, "ARM64"):
                metadata.read_build_info(self.binary)

    def test_rejects_missing_duplicate_and_wrong_segment_sections(self):
        with self.assertRaisesRegex(ValueError, "duplicate"):
            self.read(public_info(), duplicate=True)
        with self.assertRaisesRegex(ValueError, "__TEXT"):
            self.read(public_info(), owner=b"__DATA")
        data = macho(public_info()).replace(b"__maple_info", b"__other_info")
        self.binary.write_bytes(data)
        with self.assertRaisesRegex(ValueError, "no build-info"):
            metadata.read_build_info(self.binary)

    def test_rejects_duplicate_json_keys_invalid_encoding_and_trailing_payload(self):
        for payload in (b'{"profile":"dev","profile":"prod"}', b"\xff", b"{}\0", b"{}{}"):
            with self.subTest(payload=payload), self.assertRaises(ValueError):
                self.read(payload)

    def test_rejects_bad_command_sizes_counts_and_metadata_bounds(self):
        baseline = macho(public_info())
        mutations = [(20, 2 * metadata.MAX_COMMAND_BYTES), (16, 8193), (16, 2), (92, 7),
                     (36, 0), (36, 153), (96, 0),
                     (144, metadata.MAX_METADATA_BYTES + 1), (152, 0), (152, len(baseline) + 1),
                     (160, 1), (164, 1), (168, 1)]
        for offset, value in mutations:
            data = bytearray(baseline)
            struct.pack_into("<I", data, offset, value)
            self.binary.write_bytes(data)
            with self.subTest(offset=offset, value=value), self.assertRaises(ValueError):
                metadata.read_build_info(self.binary)
        for length in (0, 31, 40, len(baseline) - 1):
            self.binary.write_bytes(baseline[:length])
            with self.subTest(length=length), self.assertRaises(ValueError):
                metadata.read_build_info(self.binary)

    def test_rejects_symlinks_and_name_padding(self):
        original = self.binary.with_name("original")
        original.write_bytes(macho(public_info()))
        self.binary.symlink_to(original)
        with self.assertRaisesRegex(ValueError, "regular file"):
            metadata.read_build_info(self.binary)
        self.binary.unlink()
        data = bytearray(macho(public_info()))
        data[117] = ord("x")
        self.binary.write_bytes(data)
        with self.assertRaisesRegex(ValueError, "padding"):
            metadata.read_build_info(self.binary)

    def test_rejects_fifo_without_waiting_for_an_artifact_writer(self):
        os.mkfifo(self.binary)
        with self.assertRaisesRegex(ValueError, "regular file"):
            metadata.read_build_info(self.binary)

    def test_rejects_metadata_schema_and_endpoint_corruption(self):
        for info in (dict(public_info(), unexpected="value"), dict(public_info(), api_url="https://wrong.example"),
                     dict(public_info(), source_sha="short"), dict(public_info(), git_revision="aaaaaaaa-dirty")):
            self.binary.write_bytes(macho(info))
            with self.subTest(info=info), self.assertRaises(ValueError):
                metadata.validated_build_info("dev", self.binary, SOURCE_SHA)

    def test_cli_emits_only_canonical_json_and_newline(self):
        info = public_info()
        self.binary.write_bytes(macho(info))
        result = subprocess.run([sys.executable, str(Path(__file__).with_name("macos-build-info.py")),
                                 str(self.binary), "--profile", "dev", "--source-sha", SOURCE_SHA],
                                check=True, capture_output=True, text=True)
        self.assertEqual(result.stdout, json.dumps(info, sort_keys=True, separators=(",", ":")) + "\n")
        self.assertEqual(result.stderr, "")

    @unittest.skipUnless(sys.platform == "darwin" and shutil.which("rustc"), "native Mach-O fixture requires Darwin and Rust")
    def test_native_rust_section_survives_release_lto_and_symbol_stripping(self):
        root = self.binary.parent
        info = public_info()
        payload = json.dumps(info, sort_keys=True, separators=(",", ":")).encode()
        (root / "build-info.json").write_bytes(payload)
        source = root / "fixture.rs"
        source.write_text('''#[used]
#[unsafe(no_mangle)]
#[unsafe(link_section = "__TEXT,__maple_info")]
static MAPLE_AGENT_BUILD_INFO: [u8; include_bytes!("build-info.json").len()] = *include_bytes!("build-info.json");
fn main() { panic!("the signing preflight must never execute an artifact"); }
''')
        subprocess.run(["rustc", "--edition=2024", "-C", "opt-level=3", "-C", "lto=fat", "-C", "strip=symbols",
                        "-C", "link-arg=-Wl,-u,_MAPLE_AGENT_BUILD_INFO", str(source), "-o", str(self.binary)],
                       check=True, capture_output=True, text=True)
        self.assertEqual(metadata.validated_build_info("dev", self.binary, SOURCE_SHA), info)


if __name__ == "__main__":
    unittest.main()
