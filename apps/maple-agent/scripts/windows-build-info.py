#!/usr/bin/env python3
"""Read bounded native Windows PE metadata and imports without executing the artifact.

The layout follows https://learn.microsoft.com/windows/win32/debug/pe-format.
This is a release preflight, not an Authenticode signature verifier.
"""

import argparse
from contextlib import contextmanager
import importlib.util
import json
import os
from pathlib import Path
import re
import stat
import struct
import sys


SECTION = b".maple"
MAX_HEADER_BYTES = 1024 * 1024
MAX_METADATA_BYTES = 16 * 1024
MAX_IMPORT_BYTES = 128 * 1024
MAX_DLL_BYTES = 256
MAX_ICON_BYTES = 1024 * 1024
MACHINE_AMD64 = 0x8664
MACHINE_ARM64 = 0xAA64
MACHINES = {"x86_64": MACHINE_AMD64, "aarch64": MACHINE_ARM64}
IMAGE_FILE_EXECUTABLE_IMAGE = 0x0002
IMAGE_FILE_DLL = 0x2000
IMAGE_SCN_CNT_INITIALIZED_DATA = 0x0040
IMAGE_SCN_MEM_EXECUTE = 0x20000000
IMAGE_SCN_MEM_READ = 0x40000000
IMAGE_SCN_MEM_WRITE = 0x80000000


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError("build-info contains duplicate JSON keys")
        result[key] = value
    return result


def regular_file(status):
    return stat.S_ISREG(status.st_mode) and not (
        getattr(status, "st_file_attributes", 0) & stat.FILE_ATTRIBUTE_REPARSE_POINT
    )


@contextmanager
def open_artifact(path, arch=None):
    if arch is not None and arch not in MACHINES:
        raise ValueError("unsupported Windows architecture")
    path = Path(path)
    before = path.lstat()
    if not regular_file(before):
        raise ValueError("PE artifact must be a regular file, not a symlink or reparse point")
    flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_NONBLOCK", 0)
    flags |= getattr(os, "O_BINARY", 0)
    with os.fdopen(os.open(path, flags), "rb") as binary:
        status = os.fstat(binary.fileno())
        after = path.lstat()
        if not regular_file(status) or not regular_file(after) or (
            before.st_dev, before.st_ino
        ) != (status.st_dev, status.st_ino) or (
            after.st_dev, after.st_ino
        ) != (status.st_dev, status.st_ino):
            raise ValueError("PE artifact changed or is not a regular file")
        if not 0 < status.st_size <= 4 * 1024 * 1024 * 1024:
            raise ValueError("PE artifact is empty or oversized")
        yield PEImage(binary, status.st_size, arch)


class PEImage:
    def __init__(self, binary, size, arch=None):
        self.binary, self.size = binary, size
        dos = self.read(0, 64)
        if dos[:2] != b"MZ":
            raise ValueError("release artifact must be a supported native Windows PE executable")
        position = struct.unpack_from("<I", dos, 0x3C)[0]
        if position < 64 or position > MAX_HEADER_BYTES:
            raise ValueError("invalid PE header offset")
        header = self.read(position, 24)
        machine, count, _, _, _, optional_size, characteristics = struct.unpack_from("<HHIIIHH", header, 4)
        if header[:4] != b"PE\0\0" or machine not in MACHINES.values() or (
            not characteristics & IMAGE_FILE_EXECUTABLE_IMAGE or characteristics & IMAGE_FILE_DLL
        ):
            raise ValueError("release artifact must be a supported native Windows PE executable")
        self.arch = next(name for name, code in MACHINES.items() if code == machine)
        if arch is not None and self.arch != arch:
            raise ValueError("PE architecture does not match the selected Windows target")
        if not 1 <= count <= 96 or not 112 <= optional_size <= 4096:
            raise ValueError("invalid PE section count or optional header size")
        optional = self.read(position + 24, optional_size)
        if struct.unpack_from("<H", optional)[0] != 0x20B:
            raise ValueError("release artifact must use a PE32+ optional header")
        self.subsystem = struct.unpack_from("<H", optional, 68)[0]
        if self.subsystem not in (2, 3):
            raise ValueError("PE artifact must use the Windows GUI or console subsystem")
        image_size, headers_size = struct.unpack_from("<II", optional, 56)
        directories = struct.unpack_from("<I", optional, 108)[0]
        if directories > 16 or 112 + directories * 8 > optional_size:
            raise ValueError("invalid PE data directory count")
        self.directories = [struct.unpack_from("<II", optional, 112 + 8 * index) for index in range(directories)]
        section_table = position + 24 + optional_size
        table_end = section_table + 40 * count
        if not table_end <= headers_size <= min(self.size, MAX_HEADER_BYTES) or image_size < headers_size:
            raise ValueError("invalid PE header or image bounds")
        table = self.read(section_table, 40 * count)
        self.sections = []
        for index in range(count):
            raw_name, virtual_size, address, raw_size, offset, relocations, lines, reloc_count, line_count, flags = struct.unpack_from(
                "<8sIIIIIIHHI", table, index * 40
            )
            name, _, padding = raw_name.partition(b"\0")
            if not name or padding.strip(b"\0") or name.startswith(b"/"):
                raise ValueError("invalid PE section name")
            extent = max(virtual_size, raw_size)
            if (raw_size and (offset < headers_size or offset + raw_size > self.size)) or (
                extent and (address < headers_size or address + extent > image_size)
            ) or relocations or lines or reloc_count or line_count:
                raise ValueError("invalid PE section bounds or image relocations")
            for previous in self.sections:
                if raw_size and previous[3] and max(offset, previous[4]) < min(offset + raw_size, previous[4] + previous[3]):
                    raise ValueError("overlapping PE section file ranges")
                previous_extent = max(previous[1], previous[3])
                if extent and previous_extent and max(address, previous[2]) < min(address + extent, previous[2] + previous_extent):
                    raise ValueError("overlapping PE section virtual ranges")
            self.sections.append((name, virtual_size, address, raw_size, offset, flags))

    def read(self, offset, length):
        if offset < 0 or length < 0 or offset + length > self.size:
            raise ValueError("truncated PE artifact")
        self.binary.seek(offset)
        data = self.binary.read(length)
        if len(data) != length:
            raise ValueError("truncated PE artifact")
        return data

    def rva(self, address, length):
        for _, virtual_size, start, raw_size, offset, _ in self.sections:
            # Import data must exist in the file and mapped virtual contents,
            # rather than in raw alignment padding or zero-filled memory.
            if start <= address and address + length <= start + min(raw_size, virtual_size):
                return self.read(offset + address - start, length)
        raise ValueError("PE data RVA is outside a file-backed section")

    def dll_name(self, address):
        for _, virtual_size, start, raw_size, offset, _ in self.sections:
            available = min(raw_size, virtual_size) - (address - start)
            if start <= address and available > 0:
                data = self.read(offset + address - start, min(MAX_DLL_BYTES, available))
                name, separator, _ = data.partition(b"\0")
                if not separator or not re.fullmatch(rb"[A-Za-z0-9_+.-]+\.(?:dll|drv)", name, re.IGNORECASE) or b".." in name:
                    raise ValueError("invalid or oversized PE import DLL name")
                return name.decode("ascii").lower()
        raise ValueError("PE import name RVA is outside a file-backed section")

    def imports(self):
        names = set()
        for index, record_size, name_offset in ((1, 20, 12), (13, 32, 4)):
            if index >= len(self.directories):
                continue
            address, size = self.directories[index]
            if not address and not size:
                continue
            if not address or not record_size <= size <= MAX_IMPORT_BYTES:
                raise ValueError("invalid or oversized PE import directory")
            table = self.rva(address, size)
            for offset in range(0, size - record_size + 1, record_size):
                record = table[offset:offset + record_size]
                if not any(record):
                    break
                if index == 13 and struct.unpack_from("<I", record)[0] != 1:
                    raise ValueError("PE delay imports must use RVA addresses")
                names.add(self.dll_name(struct.unpack_from("<I", record, name_offset)[0]))
            else:
                raise ValueError("unterminated PE import directory")
        return sorted(names)

    def build_info(self):
        found = [section for section in self.sections if section[0] == SECTION]
        if len(found) != 1:
            raise ValueError("PE executable must contain exactly one build-info section")
        _, length, _, raw_size, offset, flags = found[0]
        if not flags & IMAGE_SCN_MEM_READ or not flags & IMAGE_SCN_CNT_INITIALIZED_DATA or (
            flags & (IMAGE_SCN_MEM_WRITE | IMAGE_SCN_MEM_EXECUTE)
        ):
            raise ValueError("PE build-info must be read-only initialized data")
        if not 0 < length <= MAX_METADATA_BYTES or length > raw_size or raw_size > MAX_METADATA_BYTES + 64 * 1024:
            raise ValueError("invalid or oversized PE build-info section")
        data = self.read(offset, raw_size)
        if any(data[length:]):
            raise ValueError("PE build-info alignment padding must be zero")
        return json.loads(data[:length].decode("utf-8"), object_pairs_hook=unique_object)

    def verify_icon(self, icon):
        """Match numeric ICON1 and its images without loading executable code."""
        if len(self.directories) <= 2:
            raise ValueError("PE icon resource directory is missing")
        address, size = self.directories[2]
        if not address or not 16 <= size <= MAX_ICON_BYTES:
            raise ValueError("invalid or oversized PE icon resource directory")
        resources = self.rva(address, size)

        def read(offset, length):
            if offset < 0 or offset + length > size:
                raise ValueError("PE icon resource is outside its directory")
            return resources[offset:offset + length]

        def table(pointer):
            if not pointer & 0x80000000:
                raise ValueError("PE icon resource must use a directory")
            offset = pointer & 0x7FFFFFFF
            named, count = struct.unpack_from("<HH", read(offset, 16), 12)
            if not 1 <= named + count <= 256:
                raise ValueError("invalid PE icon resource entry count")
            entries = struct.iter_unpack("<II", read(offset + 16, 8 * (named + count)))
            result = {}
            previous = -1
            for index, (key, value) in enumerate(entries):
                if bool(key & 0x80000000) != (index < named) or (index >= named and key <= previous):
                    raise ValueError("invalid PE icon resource identifier order")
                if key in result:
                    raise ValueError("duplicate PE icon resource identifier")
                result[key] = value
                previous = key if index >= named else -1
            return result

        root = table(0x80000000)

        def images(kind, identifier):
            try:
                languages = table(table(root[kind])[identifier])
            except KeyError as error:
                raise ValueError("PE icon resource identifier is missing") from error
            result = {}
            for language, pointer in languages.items():
                if language > 0xFFFF or pointer & 0x80000000:
                    raise ValueError("invalid PE icon resource language or data entry")
                rva, length, _, reserved = struct.unpack("<IIII", read(pointer, 16))
                if reserved or not 0 < length <= MAX_ICON_BYTES:
                    raise ValueError("invalid PE icon resource data size")
                read(rva - address, length)
                result[language] = self.rva(rva, length)
            return result

        if not 6 <= len(icon) <= MAX_ICON_BYTES:
            raise ValueError("invalid canonical Windows icon")
        reserved, kind, count = struct.unpack_from("<HHH", icon)
        if reserved or kind != 1 or not 1 <= count <= 16 or len(icon) < 6 + 16 * count:
            raise ValueError("invalid canonical Windows icon directory")
        expected = {}
        for index in range(count):
            width, height, colors, zero, planes, bits, length, offset = struct.unpack_from("<BBBBHHII", icon, 6 + 16 * index)
            if zero or planes not in (0, 1) or not length or offset < 6 + 16 * count or offset + length > len(icon):
                raise ValueError("invalid canonical Windows icon image")
            key = (width, height, colors, bits, length)
            if key in expected:
                raise ValueError("duplicate canonical Windows icon image")
            expected[key] = icon[offset:offset + length]
        for language, group in images(14, 1).items():
            if len(group) != 6 + 14 * count or struct.unpack_from("<HHH", group) != (0, 1, count):
                raise ValueError("PE ICON1 differs from its canonical profile icon")
            actual = {}
            for index in range(count):
                width, height, colors, zero, planes, bits, length, identifier = struct.unpack_from("<BBBBHHIH", group, 6 + 14 * index)
                key = (width, height, colors, bits, length)
                if zero or planes not in (0, 1) or not identifier or key in actual:
                    raise ValueError("invalid PE ICON1 image descriptor")
                actual[key] = images(3, identifier).get(language)
            if actual != expected:
                raise ValueError("PE ICON1 differs from its canonical profile icon")


def inspect_pe(path):
    """Return native architecture, subsystem and all regular/delay DLL imports."""
    with open_artifact(path) as image:
        return {"machine": image.arch, "subsystem": image.subsystem, "imports": image.imports()}


def read_build_info(path):
    with open_artifact(path) as image:
        return image.build_info()


def verify_icon(path, icon_path):
    with open_artifact(path) as image:
        image.verify_icon(Path(icon_path).read_bytes())


def validated_build_info(profile, path, source_sha, arch="x86_64"):
    if not isinstance(source_sha, str) or not re.fullmatch(r"[0-9a-f]{40}", source_sha):
        raise ValueError("expected source commit must be a full lowercase SHA")
    spec = importlib.util.spec_from_file_location("release_info", Path(__file__).with_name("release-info.py"))
    release_info = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(release_info)
    with open_artifact(path, arch) as image:
        info = image.build_info()
    return release_info.validate_info(profile, info, source_sha, platform="windows-" + arch)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary")
    parser.add_argument("--profile", choices=("dev", "prod"), required=True)
    parser.add_argument("--source-sha", required=True)
    parser.add_argument("--arch", choices=tuple(MACHINES), default="x86_64")
    args = parser.parse_args()
    print(json.dumps(validated_build_info(args.profile, args.binary, args.source_sha, args.arch), sort_keys=True, separators=(",", ":")))


if __name__ == "__main__":
    try:
        main()
    except (ValueError, KeyError, OSError, UnicodeDecodeError, struct.error) as error:
        print(f"static Windows build-info validation failed: {error}", file=sys.stderr)
        sys.exit(1)
