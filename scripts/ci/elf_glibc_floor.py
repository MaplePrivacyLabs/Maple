#!/usr/bin/env python3
"""Fail when a Linux ELF file needs glibc symbol versions above the supported floor.

Research's Linux packages run on the host's glibc, so the newest GLIBC_x.y
version a packaged executable references is the oldest distribution it can
start on. The floor is the glibc of the oldest supported distribution.
"""

import argparse
from pathlib import Path
import struct
import sys


SHT_DYNSYM = 11
SHT_GNU_VERNEED = 0x6FFFFFFE
SHT_GNU_VERSYM = 0x6FFFFFFF
VERSION_PREFIX = "GLIBC_"


def parse_version(text):
    try:
        parts = tuple(int(part) for part in text.split("."))
    except ValueError as error:
        raise ValueError(f"invalid glibc version {text!r}") from error
    if len(parts) < 2 or any(part < 0 for part in parts):
        raise ValueError(f"invalid glibc version {text!r}")
    return parts


def format_version(parts):
    return ".".join(str(part) for part in parts)


class Elf64:
    def __init__(self, data):
        if len(data) < 64 or data[:4] != b"\x7fELF":
            raise ValueError("not an ELF file")
        if data[4] != 2:
            raise ValueError("only ELF64 files are supported")
        if data[5] != 1:
            raise ValueError("only little-endian ELF files are supported")
        self.data = data
        shoff, = struct.unpack_from("<Q", data, 0x28)
        shentsize, shnum, shstrndx = struct.unpack_from("<HHH", data, 0x3A)
        if shentsize < 64:
            raise ValueError("invalid section header size")
        self.sections = []
        for index in range(shnum):
            offset = shoff + index * shentsize
            if offset + 64 > len(data):
                raise ValueError("truncated section header table")
            name, kind, _flags, _addr, off, size, link, info, _align, entsize = struct.unpack_from(
                "<IIQQQQIIQQ", data, offset)
            self.sections.append({"name": name, "type": kind, "offset": off, "size": size,
                                  "link": link, "info": info, "entsize": entsize})
        self.shstrndx = shstrndx

    def section_of_type(self, kind):
        matches = [section for section in self.sections if section["type"] == kind]
        if len(matches) > 1:
            raise ValueError(f"multiple sections of type {kind:#x}")
        return matches[0] if matches else None

    def string(self, section, offset):
        start = section["offset"] + offset
        end = self.data.find(b"\0", start)
        if start > len(self.data) or end < 0:
            raise ValueError("string table read out of bounds")
        return self.data[start:end].decode("utf-8", "replace")

    def bytes_of(self, section):
        start, size = section["offset"], section["size"]
        if start + size > len(self.data):
            raise ValueError("section data out of bounds")
        return self.data[start:start + size]


def needed_versions(elf):
    """Map version index -> (library, version name) from .gnu.version_r."""
    verneed = elf.section_of_type(SHT_GNU_VERNEED)
    if verneed is None:
        return {}
    strings = elf.sections[verneed["link"]]
    versions = {}
    offset = verneed["offset"]
    for _ in range(verneed["info"]):
        vn_version, vn_cnt, vn_file, vn_aux, vn_next = struct.unpack_from("<HHIII", elf.data, offset)
        if vn_version != 1:
            raise ValueError(f"unsupported verneed entry version {vn_version}")
        library = elf.string(strings, vn_file)
        aux = offset + vn_aux
        for _ in range(vn_cnt):
            _hash, _flags, other, name, aux_next = struct.unpack_from("<IHHII", elf.data, aux)
            versions[other] = (library, elf.string(strings, name))
            aux += aux_next
        if vn_next == 0:
            break
        offset += vn_next
    return versions


def symbols_by_version(elf, versions):
    """Map version index -> sorted undefined symbol names bound to it."""
    dynsym = elf.section_of_type(SHT_DYNSYM)
    versym = elf.section_of_type(SHT_GNU_VERSYM)
    if dynsym is None or versym is None:
        return {}
    strings = elf.sections[dynsym["link"]]
    entsize = dynsym["entsize"] or 24
    count = dynsym["size"] // entsize
    versym_data = elf.bytes_of(versym)
    result = {}
    for index in range(min(count, len(versym_data) // 2)):
        name, _info, _other, shndx, _value, _size = struct.unpack_from(
            "<IBBHQQ", elf.data, dynsym["offset"] + index * entsize)
        version_index, = struct.unpack_from("<H", versym_data, index * 2)
        version_index &= 0x7FFF
        if shndx == 0 and version_index in versions:
            result.setdefault(version_index, []).append(elf.string(strings, name))
    return {index: sorted(names) for index, names in result.items()}


def glibc_requirements(path):
    """Return {(library, version tuple): [symbols]} for every GLIBC_ version needed."""
    elf = Elf64(Path(path).read_bytes())
    versions = needed_versions(elf)
    symbols = symbols_by_version(elf, versions)
    requirements = {}
    for index, (library, name) in versions.items():
        if not name.startswith(VERSION_PREFIX):
            continue
        key = (library, parse_version(name[len(VERSION_PREFIX):]))
        requirements.setdefault(key, []).extend(symbols.get(index, []))
    return requirements


def check(path, floor):
    requirements = glibc_requirements(path)
    newest = max((version for _, version in requirements), default=None)
    above = {key: names for key, names in requirements.items() if key[1] > floor}
    return newest, above


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--floor", required=True, type=parse_version,
                        help="highest glibc symbol version the file may require, e.g. 2.39")
    parser.add_argument("files", nargs="+", type=Path)
    args = parser.parse_args()

    status = 0
    for path in args.files:
        try:
            newest, above = check(path, args.floor)
        except (OSError, ValueError) as error:
            print(f"glibc floor check failed: {path}: {error}", file=sys.stderr)
            status = 1
            continue
        if above:
            status = 1
            print(f"{path} needs glibc symbol versions above the {format_version(args.floor)} floor:",
                  file=sys.stderr)
            for (library, version), names in sorted(above.items()):
                listed = ", ".join(names) if names else "<no undefined symbols listed>"
                print(f"  {library} {VERSION_PREFIX}{format_version(version)}: {listed}", file=sys.stderr)
            continue
        print(f"verified-linux-glibc-floor  {VERSION_PREFIX}{format_version(newest) if newest else 'none'}"
              f"  floor={format_version(args.floor)}  {path}")
    return status


if __name__ == "__main__":
    sys.exit(main())
