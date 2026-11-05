#!/usr/bin/env python3
"""Fail when a Linux ELF file needs glibc symbol versions above the supported floor.

Research's Linux packages run on the host's glibc, so the newest GLIBC_x.y
version a packaged executable references is the oldest distribution it can
start on. The floor is the glibc of the oldest supported distribution.

--require-fortify also fails a file with no fortified glibc import. The floor's
zig compiler applies no C hardening unless asked, and the stack protector
leaves no import to check (zig's compiler-rt supplies __stack_chk_fail), so a
glibc __*_chk import is the evidence that the hardening flags reached the C
dependencies.

--summary prints one line for the whole set instead of one per file, and
--exact also fails when no file needs the floor itself, so a declared floor
cannot drift above what the files really need.
"""

import argparse
from pathlib import Path
import struct
import sys


SHT_DYNSYM = 11
SHT_GNU_VERNEED = 0x6FFFFFFE
SHT_GNU_VERSYM = 0x6FFFFFFF
VERSION_PREFIX = "GLIBC_"
ABI_PREFIX = "GLIBC_ABI_"
# The linker adds GLIBC_ABI_* markers for loader features, so that a glibc
# without the feature refuses the file. Distributions backport them, so they do
# not follow release order: list each with the first release whose every build
# defines it. Any other marker counts as above the floor.
ABI_MARKER_SINCE = {
    "GLIBC_ABI_DT_RELR": (2, 36),
    "GLIBC_ABI_DT_X86_64_PLT": (2, 43),
    "GLIBC_ABI_GNU2_TLS": (2, 43),
}


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


def dynamic_symbols(elf):
    """Yield (index, name, is_undefined) for every .dynsym entry."""
    dynsym = elf.section_of_type(SHT_DYNSYM)
    if dynsym is None:
        return
    strings = elf.sections[dynsym["link"]]
    entsize = dynsym["entsize"] or 24
    for index in range(dynsym["size"] // entsize):
        name, _info, _other, shndx, _value, _size = struct.unpack_from(
            "<IBBHQQ", elf.data, dynsym["offset"] + index * entsize)
        yield index, elf.string(strings, name), shndx == 0


def symbols_by_version(elf, versions):
    """Map version index -> sorted undefined symbol names bound to it."""
    versym = elf.section_of_type(SHT_GNU_VERSYM)
    if versym is None:
        return {}
    versym_data = elf.bytes_of(versym)
    result = {}
    for index, name, undefined in dynamic_symbols(elf):
        if not undefined or index * 2 + 2 > len(versym_data):
            continue
        version_index, = struct.unpack_from("<H", versym_data, index * 2)
        version_index &= 0x7FFF
        if version_index in versions:
            result.setdefault(version_index, []).append(name)
    return {index: sorted(names) for index, names in result.items()}


def glibc_requirements(elf):
    """Return {(library, version name): [symbols]} for every GLIBC_ version needed."""
    versions = needed_versions(elf)
    symbols = symbols_by_version(elf, versions)
    requirements = {}
    for index, (library, name) in versions.items():
        if name.startswith(VERSION_PREFIX):
            requirements.setdefault((library, name), []).extend(symbols.get(index, []))
    return requirements


def required_release(name):
    """The glibc release a version name needs, or None for an unknown ABI marker."""
    if name.startswith(ABI_PREFIX):
        return ABI_MARKER_SINCE.get(name)
    return parse_version(name[len(VERSION_PREFIX):])


def fortified_imports(elf):
    """The glibc _FORTIFY_SOURCE entry points the file imports."""
    return sorted(name for _, name, is_undefined in dynamic_symbols(elf)
                  if is_undefined and name.startswith("__") and name.endswith("_chk"))


def check(path, floor):
    """Return (newest numbered release needed, {(library, name): symbols} above the floor, fortified imports)."""
    elf = Elf64(Path(path).read_bytes())
    newest = None
    above = {}
    for (library, name), names in glibc_requirements(elf).items():
        release = required_release(name)
        if release is None or release > floor:
            above[(library, name)] = names
        if release is not None and not name.startswith(ABI_PREFIX):
            newest = release if newest is None else max(newest, release)
    return newest, above, fortified_imports(elf)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--floor", required=True, type=parse_version,
                        help="highest glibc symbol version the file may require, e.g. 2.39")
    parser.add_argument("--require-fortify", action="store_true",
                        help="also require at least one fortified glibc __*_chk import")
    parser.add_argument("--exact", action="store_true",
                        help="also fail when no file needs the floor release itself")
    parser.add_argument("--summary", metavar="LABEL",
                        help="print one line for all files, named LABEL, instead of one per file")
    parser.add_argument("files", nargs="+", type=Path)
    args = parser.parse_args()
    floor = format_version(args.floor)

    status = 0
    newest_overall = None
    for path in args.files:
        try:
            newest, above, fortified = check(path, args.floor)
        except (OSError, ValueError, struct.error) as error:
            print(f"glibc floor check failed: {path}: {error}", file=sys.stderr)
            status = 1
            continue
        if above:
            print(f"{path} needs glibc symbol versions above the {floor} floor:", file=sys.stderr)
            for (library, name), names in sorted(above.items()):
                if name.startswith(ABI_PREFIX):
                    listed = "loader ABI marker the floor's glibc does not define"
                else:
                    listed = ", ".join(names) if names else "<no undefined symbols listed>"
                print(f"  {library} {name}: {listed}", file=sys.stderr)
        unfortified = args.require_fortify and not fortified
        if unfortified:
            print(f"{path} imports no fortified glibc __*_chk function: "
                  "its C code was built without the hardening flags", file=sys.stderr)
        if above or unfortified:
            status = 1
            continue
        if newest is not None:
            newest_overall = newest if newest_overall is None else max(newest_overall, newest)
        if args.summary is None:
            fortify = f"  fortify={len(fortified)}" if args.require_fortify else ""
            print(f"verified-linux-glibc-floor  {VERSION_PREFIX}{format_version(newest) if newest else 'none'}"
                  f"  floor={floor}{fortify}  {path}")

    label = args.summary if args.summary is not None else "the files"
    if status == 0 and args.exact and newest_overall != args.floor:
        needed = format_version(newest_overall) if newest_overall else "none"
        print(f"{label}: the newest glibc release needed is {needed}, below the declared {floor} floor; "
              "lower the declared floor to match", file=sys.stderr)
        status = 1
    if status == 0 and args.summary is not None:
        print(f"verified-linux-glibc-floor  {VERSION_PREFIX}{format_version(newest_overall) if newest_overall else 'none'}"
              f"  floor={floor}  files={len(args.files)}  {args.summary}")
    return status


if __name__ == "__main__":
    sys.exit(main())
