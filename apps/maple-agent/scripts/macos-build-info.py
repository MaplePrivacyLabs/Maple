#!/usr/bin/env python3
"""Read the public build-info section without executing a Mach-O artifact."""

import argparse
import importlib.util
import json
import os
from pathlib import Path
import re
import stat
import struct
import sys


MH_MAGIC_64 = 0xFEEDFACF
CPU_TYPE_ARM64 = 0x0100000C
MH_EXECUTE = 2
LC_SEGMENT_64 = 0x19
SEGMENT = b"__TEXT"
SECTION = b"__maple_info"
MAX_COMMAND_BYTES = 1024 * 1024
MAX_METADATA_BYTES = 16 * 1024


def name(raw):
    """Mach-O names are fixed-size, zero-padded ASCII fields."""
    value, _, padding = raw.partition(b"\0")
    if padding.strip(b"\0"):
        raise ValueError("Mach-O section or segment name has invalid padding")
    return value


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError("build-info contains duplicate JSON keys")
        result[key] = value
    return result


def read_build_info(path):
    """Parse only a bounded header, load-command table, and metadata section.

    Releases currently support thin ARM64 executables. Reject fat binaries,
    alternative architectures, and ambiguous sections rather than guessing
    which image or metadata a signing job should trust.
    """
    path = Path(path)
    if path.is_symlink():
        raise ValueError("Mach-O artifact must be a regular file, not a symlink")
    # Nonblocking open rejects FIFOs/devices before reading, and NOFOLLOW
    # prevents a link substitution between the path check and opening it.
    with os.fdopen(os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK), "rb") as binary:
        status = os.fstat(binary.fileno())
        if path.is_symlink() or not stat.S_ISREG(status.st_mode):
            raise ValueError("Mach-O artifact must be a regular file, not a symlink")
        header = binary.read(32)
        if len(header) != 32:
            raise ValueError("truncated Mach-O header")
        magic, cpu, subtype, kind, count, size, _, reserved = struct.unpack("<8I", header)
        if magic != MH_MAGIC_64 or cpu != CPU_TYPE_ARM64 or subtype != 0 or kind != MH_EXECUTE:
            raise ValueError("release artifact must be a thin ARM64 Mach-O executable")
        if reserved or not count or count > 8192 or size > MAX_COMMAND_BYTES or 32 + size > status.st_size:
            raise ValueError("invalid or oversized Mach-O load-command table")
        commands = binary.read(size)
        found = None
        position = 0
        for _ in range(count):
            if position + 8 > len(commands):
                raise ValueError("truncated Mach-O load command")
            command, command_size = struct.unpack_from("<2I", commands, position)
            end = position + command_size
            if command_size < 8 or command_size % 8 or end > len(commands):
                raise ValueError("invalid Mach-O load-command size")
            if command == LC_SEGMENT_64:
                if command_size < 72:
                    raise ValueError("truncated Mach-O segment command")
                segment, address, vm_size, file_offset, file_size, _, protection, sections, _ = struct.unpack_from(
                    "<16s4Q4I", commands, position + 8
                )
                segment = name(segment)
                if command_size != 72 + sections * 80 or file_offset + file_size > status.st_size:
                    raise ValueError("invalid Mach-O segment bounds or section count")
                for index in range(sections):
                    section, owner, location, length, offset, alignment, relocation, relocations, flags, first, second, third = struct.unpack_from(
                        "<16s16s2Q8I", commands, position + 72 + index * 80
                    )
                    section, owner = name(section), name(owner)
                    if section != SECTION:
                        continue
                    if found is not None:
                        raise ValueError("duplicate Mach-O build-info section")
                    if segment != SEGMENT or owner != SEGMENT:
                        raise ValueError("Mach-O build-info must be in __TEXT")
                    if not protection & 1 or protection & 2:
                        raise ValueError("Mach-O build-info must be in a read-only segment")
                    if (
                        not length or length > MAX_METADATA_BYTES or flags & 0xFF or alignment > 31
                        or relocation or relocations or first or second or third
                        or offset < 32 + size or offset < file_offset
                        or offset + length > file_offset + file_size
                        or location != address + offset - file_offset
                        or location + length > address + vm_size
                    ):
                        raise ValueError("invalid Mach-O build-info section bounds or type")
                    found = offset, length
            position = end
        if position != len(commands):
            raise ValueError("Mach-O load-command table contains trailing data")
        if found is None:
            raise ValueError("Mach-O executable has no build-info section")
        binary.seek(found[0])
        payload = binary.read(found[1])
        if len(payload) != found[1]:
            raise ValueError("truncated Mach-O build-info section")
    return json.loads(payload.decode("utf-8"), object_pairs_hook=unique_object)


def validated_build_info(profile, path, source_sha):
    if not isinstance(source_sha, str) or not re.fullmatch(r"[0-9a-f]{40}", source_sha):
        raise ValueError("expected source commit must be a full lowercase SHA")
    spec = importlib.util.spec_from_file_location("release_info", Path(__file__).with_name("release-info.py"))
    release_info = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(release_info)
    return release_info.validate_info(profile, read_build_info(path), source_sha)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary")
    parser.add_argument("--profile", choices=("dev", "prod"), required=True)
    parser.add_argument("--source-sha", required=True)
    args = parser.parse_args()
    print(json.dumps(validated_build_info(args.profile, args.binary, args.source_sha), sort_keys=True, separators=(",", ":")))


if __name__ == "__main__":
    try:
        main()
    except (ValueError, KeyError, OSError, UnicodeDecodeError, struct.error) as error:
        print(f"static macOS build-info validation failed: {error}", file=sys.stderr)
        sys.exit(1)
