#!/usr/bin/env python3
import hashlib
import json
import os
import plistlib
import shutil
import subprocess
import struct
import sys
import tempfile


VOLATILE_INFO_PLIST_KEYS = {
    "BuildMachineOSBuild",
    "DTCompiler",
    "DTPlatformBuild",
    "DTPlatformName",
    "DTPlatformVersion",
    "DTSDKBuild",
    "DTSDKName",
    "DTXcode",
    "DTXcodeBuild",
}

VOLATILE_ASSET_KEYS = {
    "AssetStorageVersion",
    "Authoring Tool",
    "CoreUIVersion",
    "DumpToolVersion",
    "MainVersion",
    "Timestamp",
}

VOLATILE_FILENAMES = {
    "archived-expanded-entitlements.xcent",
    "CodeResources",
    "embedded.mobileprovision",
    "embedded.provisionprofile",
}

MACHO_MAGICS = {
    b"\xfe\xed\xfa\xce",
    b"\xce\xfa\xed\xfe",
    b"\xfe\xed\xfa\xcf",
    b"\xcf\xfa\xed\xfe",
    b"\xca\xfe\xba\xbe",
    b"\xbe\xba\xfe\xca",
}

LC_UUID = 0x1B
LC_SEGMENT = 0x1
LC_SEGMENT_64 = 0x19
FAT_ARCH_SIZE = 20
FAT_ARCH_64_SIZE = 32


def sha256_bytes(data):
    return hashlib.sha256(data).hexdigest()


def normalize_json(value, volatile_keys):
    if isinstance(value, dict):
        return {
            key: normalize_json(value[key], volatile_keys)
            for key in sorted(value)
            if key not in volatile_keys
        }
    if isinstance(value, list):
        normalized = [normalize_json(item, volatile_keys) for item in value]
        return sorted(
            normalized,
            key=lambda item: json.dumps(item, sort_keys=True, separators=(",", ":")),
        )
    return value


def canonical_info_plist(path):
    with open(path, "rb") as f:
        value = plistlib.load(f)
    for key in VOLATILE_INFO_PLIST_KEYS:
        value.pop(key, None)
    if "iPhoneOS" in value.get("CFBundleSupportedPlatforms", []):
        # App Store Connect export can rewrite this; the final IPA hash still covers it.
        value.pop("CFBundleVersion", None)
        if (isinstance(value.get("CFBundleSupportedPlatforms"), list)
                and value.get("TFInternalTestingOnly") is True):
            # Xcode adds this exact boolean during internal-only TestFlight
            # export. Dev IPA verification separately requires the restriction;
            # malformed/false values and non-iOS metadata remain significant.
            value.pop("TFInternalTestingOnly")
    url_types = value.get("CFBundleURLTypes")
    if isinstance(url_types, list):
        # Xcode export can add empty URL type entries, which do not register a scheme.
        url_types = [
            entry
            for entry in url_types
            if not isinstance(entry, dict) or entry.get("CFBundleURLSchemes")
        ]
        if url_types:
            value["CFBundleURLTypes"] = url_types
        else:
            value.pop("CFBundleURLTypes", None)
    return plistlib.dumps(value, fmt=plistlib.FMT_XML, sort_keys=True)


def canonical_assets_car(path):
    output = subprocess.check_output(["assetutil", "--info", path], text=True)
    value = json.loads(output)
    normalized = normalize_json(value, VOLATILE_ASSET_KEYS)
    return json.dumps(normalized, sort_keys=True, separators=(",", ":")).encode()


def is_macho(path):
    with open(path, "rb") as f:
        return f.read(4) in MACHO_MAGICS


def developer_tool(name):
    try:
        path = subprocess.check_output(["xcrun", "--find", name], text=True).strip()
        if path:
            return path
    except (subprocess.CalledProcessError, FileNotFoundError):
        pass
    return name


def zero_macho_uuids(data, base=0, size=None):
    if size is None:
        size = len(data) - base
    if size < 4:
        return

    magic = bytes(data[base : base + 4])

    if magic in {b"\xca\xfe\xba\xbe", b"\xbe\xba\xfe\xca", b"\xca\xfe\xba\xbf", b"\xbf\xba\xfe\xca"}:
        endian = ">" if magic in {b"\xca\xfe\xba\xbe", b"\xca\xfe\xba\xbf"} else "<"
        arch_size = FAT_ARCH_64_SIZE if magic in {b"\xca\xfe\xba\xbf", b"\xbf\xba\xfe\xca"} else FAT_ARCH_SIZE
        if size < 8:
            return

        nfat_arch = struct.unpack_from(f"{endian}I", data, base + 4)[0]
        arch_base = base + 8
        for index in range(nfat_arch):
            entry = arch_base + index * arch_size
            if entry + arch_size > base + size:
                return

            if arch_size == FAT_ARCH_64_SIZE:
                offset, arch_slice_size = struct.unpack_from(f"{endian}QQ", data, entry + 8)
            else:
                offset, arch_slice_size = struct.unpack_from(f"{endian}II", data, entry + 8)

            if offset <= len(data) and arch_slice_size <= len(data) - offset:
                zero_macho_uuids(data, offset, arch_slice_size)
        return

    if magic in {b"\xfe\xed\xfa\xce", b"\xfe\xed\xfa\xcf"}:
        endian = ">"
    elif magic in {b"\xce\xfa\xed\xfe", b"\xcf\xfa\xed\xfe"}:
        endian = "<"
    else:
        return

    is_64_bit = magic in {b"\xfe\xed\xfa\xcf", b"\xcf\xfa\xed\xfe"}
    header_size = 32 if is_64_bit else 28
    if size < header_size:
        return

    ncmds = struct.unpack_from(f"{endian}I", data, base + 16)[0]
    command = base + header_size
    limit = base + size

    for _ in range(ncmds):
        if command + 8 > limit:
            return

        cmd, cmdsize = struct.unpack_from(f"{endian}II", data, command)
        if cmdsize < 8 or command + cmdsize > limit:
            return

        if cmd == LC_UUID and cmdsize >= 24:
            data[command + 8 : command + 24] = b"\0" * 16

        command += cmdsize


def decode_macho_name(raw):
    return raw.split(b"\0", 1)[0].decode("ascii", errors="ignore")


def zero_linker_stubs(data, base=0, size=None):
    if size is None:
        size = len(data) - base
    if size < 4:
        return

    magic = bytes(data[base : base + 4])

    if magic in {b"\xca\xfe\xba\xbe", b"\xbe\xba\xfe\xca", b"\xca\xfe\xba\xbf", b"\xbf\xba\xfe\xca"}:
        endian = ">" if magic in {b"\xca\xfe\xba\xbe", b"\xca\xfe\xba\xbf"} else "<"
        arch_size = FAT_ARCH_64_SIZE if magic in {b"\xca\xfe\xba\xbf", b"\xbf\xba\xfe\xca"} else FAT_ARCH_SIZE
        if size < 8:
            return

        nfat_arch = struct.unpack_from(f"{endian}I", data, base + 4)[0]
        arch_base = base + 8
        for index in range(nfat_arch):
            entry = arch_base + index * arch_size
            if entry + arch_size > base + size:
                return

            if arch_size == FAT_ARCH_64_SIZE:
                offset, arch_slice_size = struct.unpack_from(f"{endian}QQ", data, entry + 8)
            else:
                offset, arch_slice_size = struct.unpack_from(f"{endian}II", data, entry + 8)

            if offset <= len(data) and arch_slice_size <= len(data) - offset:
                zero_linker_stubs(data, offset, arch_slice_size)
        return

    if magic in {b"\xfe\xed\xfa\xce", b"\xfe\xed\xfa\xcf"}:
        endian = ">"
    elif magic in {b"\xce\xfa\xed\xfe", b"\xcf\xfa\xed\xfe"}:
        endian = "<"
    else:
        return

    is_64_bit = magic in {b"\xfe\xed\xfa\xcf", b"\xcf\xfa\xed\xfe"}
    header_size = 32 if is_64_bit else 28
    if size < header_size:
        return

    ncmds = struct.unpack_from(f"{endian}I", data, base + 16)[0]
    command = base + header_size
    limit = base + size

    for _ in range(ncmds):
        if command + 8 > limit:
            return

        cmd, cmdsize = struct.unpack_from(f"{endian}II", data, command)
        if cmdsize < 8 or command + cmdsize > limit:
            return

        if cmd in {LC_SEGMENT, LC_SEGMENT_64}:
            is_segment_64 = cmd == LC_SEGMENT_64
            segment_header_size = 72 if is_segment_64 else 56
            section_size = 80 if is_segment_64 else 68
            if cmdsize >= segment_header_size:
                nsects = struct.unpack_from(f"{endian}I", data, command + (64 if is_segment_64 else 48))[0]
                section = command + segment_header_size

                for _ in range(nsects):
                    if section + section_size > command + cmdsize:
                        break

                    sectname = decode_macho_name(bytes(data[section : section + 16]))
                    segname = decode_macho_name(bytes(data[section + 16 : section + 32]))

                    if is_segment_64:
                        section_data_size = struct.unpack_from(f"{endian}Q", data, section + 40)[0]
                        section_offset = struct.unpack_from(f"{endian}I", data, section + 48)[0]
                    else:
                        section_data_size = struct.unpack_from(f"{endian}I", data, section + 36)[0]
                        section_offset = struct.unpack_from(f"{endian}I", data, section + 40)[0]

                    # Linker-generated stubs can choose between duplicate lazy
                    # bind slots for the same symbol. The symbol and bind
                    # metadata remain hashed elsewhere in the Mach-O image.
                    if segname == "__TEXT" and sectname in {"__stubs", "__objc_stubs"}:
                        start = base + section_offset
                        end = start + section_data_size
                        if base <= start <= end <= base + size:
                            data[start:end] = b"\0" * section_data_size

                    section += section_size

        command += cmdsize


def sign_extend(value, bits):
    sign_bit = 1 << (bits - 1)
    return (value ^ sign_bit) - sign_bit


def duplicate_got_fixup_addresses(path):
    try:
        output = subprocess.check_output(
            [developer_tool("dyld_info"), "-fixups", path],
            text=True,
            stderr=subprocess.DEVNULL,
        )
    except (subprocess.CalledProcessError, FileNotFoundError):
        return {}

    bind_addresses_by_target = {}
    for line in output.splitlines():
        parts = line.split()
        if len(parts) < 5 or parts[1] != "__got" or parts[3] != "bind":
            continue

        try:
            address = int(parts[2], 16)
        except ValueError:
            continue

        target = " ".join(parts[4:])
        bind_addresses_by_target.setdefault(target, set()).add(address)

    canonical_by_address = {}
    for addresses in bind_addresses_by_target.values():
        sorted_addresses = sorted(addresses)
        if len(sorted_addresses) < 2:
            continue

        canonical_address = sorted_addresses[0]
        for address in sorted_addresses:
            canonical_by_address[address] = canonical_address

    return canonical_by_address


def is_adrp_instruction(instruction):
    return (instruction & 0x9F000000) == 0x90000000


def adrp_register(instruction):
    return instruction & 0x1F


def adrp_page_address(instruction, pc):
    immlo = (instruction >> 29) & 0x3
    immhi = (instruction >> 5) & 0x7FFFF
    immediate = sign_extend((immhi << 2) | immlo, 21)
    return (pc & ~0xFFF) + (immediate << 12)


def is_ldr_64_unsigned_immediate(instruction):
    return (instruction & 0xFFC00000) == 0xF9400000


def normalize_duplicate_got_loads(data, canonical_got_address_by_address, base=0, size=None):
    if not canonical_got_address_by_address:
        return

    if size is None:
        size = len(data) - base
    if size < 4:
        return

    magic = bytes(data[base : base + 4])

    if magic in {b"\xca\xfe\xba\xbe", b"\xbe\xba\xfe\xca", b"\xca\xfe\xba\xbf", b"\xbf\xba\xfe\xca"}:
        endian = ">" if magic in {b"\xca\xfe\xba\xbe", b"\xca\xfe\xba\xbf"} else "<"
        arch_size = FAT_ARCH_64_SIZE if magic in {b"\xca\xfe\xba\xbf", b"\xbf\xba\xfe\xca"} else FAT_ARCH_SIZE
        if size < 8:
            return

        nfat_arch = struct.unpack_from(f"{endian}I", data, base + 4)[0]
        arch_base = base + 8
        for index in range(nfat_arch):
            entry = arch_base + index * arch_size
            if entry + arch_size > base + size:
                return

            if arch_size == FAT_ARCH_64_SIZE:
                offset, arch_slice_size = struct.unpack_from(f"{endian}QQ", data, entry + 8)
            else:
                offset, arch_slice_size = struct.unpack_from(f"{endian}II", data, entry + 8)

            if offset <= len(data) and arch_slice_size <= len(data) - offset:
                normalize_duplicate_got_loads(
                    data,
                    canonical_got_address_by_address,
                    offset,
                    arch_slice_size,
                )
        return

    if magic in {b"\xfe\xed\xfa\xce", b"\xfe\xed\xfa\xcf"}:
        endian = ">"
    elif magic in {b"\xce\xfa\xed\xfe", b"\xcf\xfa\xed\xfe"}:
        endian = "<"
    else:
        return

    is_64_bit = magic in {b"\xfe\xed\xfa\xcf", b"\xcf\xfa\xed\xfe"}
    if not is_64_bit:
        return

    header_size = 32
    if size < header_size:
        return

    ncmds = struct.unpack_from(f"{endian}I", data, base + 16)[0]
    command = base + header_size
    limit = base + size

    for _ in range(ncmds):
        if command + 8 > limit:
            return

        cmd, cmdsize = struct.unpack_from(f"{endian}II", data, command)
        if cmdsize < 8 or command + cmdsize > limit:
            return

        if cmd == LC_SEGMENT_64 and cmdsize >= 72:
            nsects = struct.unpack_from(f"{endian}I", data, command + 64)[0]
            section = command + 72

            for _ in range(nsects):
                if section + 80 > command + cmdsize:
                    break

                sectname = decode_macho_name(bytes(data[section : section + 16]))
                segname = decode_macho_name(bytes(data[section + 16 : section + 32]))
                section_address = struct.unpack_from(f"{endian}Q", data, section + 32)[0]
                section_size = struct.unpack_from(f"{endian}Q", data, section + 40)[0]
                section_offset = struct.unpack_from(f"{endian}I", data, section + 48)[0]

                if segname == "__TEXT" and sectname == "__text":
                    start = base + section_offset
                    end = start + section_size
                    if base <= start <= end <= base + size:
                        normalize_duplicate_got_loads_in_text_section(
                            data,
                            start,
                            section_size,
                            section_address,
                            canonical_got_address_by_address,
                        )

                section += 80

        command += cmdsize


def normalize_duplicate_got_loads_in_text_section(
    data,
    section_offset,
    section_size,
    section_address,
    canonical_got_address_by_address,
):
    if section_size < 8:
        return

    section_end = section_offset + section_size
    for instruction_offset in range(section_offset + 4, section_end - 3, 4):
        instruction = struct.unpack_from("<I", data, instruction_offset)[0]
        if not is_ldr_64_unsigned_immediate(instruction):
            continue

        previous_instruction = struct.unpack_from("<I", data, instruction_offset - 4)[0]
        if not is_adrp_instruction(previous_instruction):
            continue

        base_register = (instruction >> 5) & 0x1F
        if adrp_register(previous_instruction) != base_register:
            continue

        pc = section_address + (instruction_offset - 4 - section_offset)
        page_address = adrp_page_address(previous_instruction, pc)
        scaled_offset = ((instruction >> 10) & 0xFFF) * 8
        target_address = page_address + scaled_offset
        canonical_address = canonical_got_address_by_address.get(target_address)
        if canonical_address is None or canonical_address == target_address:
            continue

        # Xcode/ld can select any duplicate GOT slot for the same imported
        # symbol, so canonicalize the load to the lowest equivalent slot.
        canonical_scaled_offset = canonical_address - page_address
        if canonical_scaled_offset < 0 or canonical_scaled_offset > 0xFFF * 8:
            continue
        if canonical_scaled_offset % 8 != 0:
            continue

        canonical_immediate = canonical_scaled_offset // 8
        canonical_instruction = (instruction & ~(0xFFF << 10)) | (canonical_immediate << 10)
        struct.pack_into("<I", data, instruction_offset, canonical_instruction)


def canonical_macho(path):
    with tempfile.NamedTemporaryFile(delete=False) as tmp:
        tmp_path = tmp.name

    try:
        shutil.copyfile(path, tmp_path)
        # Local symbols are not runtime content, and ld may emit otherwise
        # identical local absolute symbols in a different order.
        subprocess.run(
            [developer_tool("strip"), "-S", "-x", tmp_path],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=True,
        )
        subprocess.run(
            ["codesign", "--remove-signature", tmp_path],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        with open(tmp_path, "rb") as f:
            data = bytearray(f.read())
        canonical_got_address_by_address = duplicate_got_fixup_addresses(tmp_path)
        zero_macho_uuids(data)
        zero_linker_stubs(data)
        normalize_duplicate_got_loads(data, canonical_got_address_by_address)
        return bytes(data)
    finally:
        try:
            os.unlink(tmp_path)
        except FileNotFoundError:
            pass


def canonical_file_bytes(root, relpath):
    path = os.path.join(root, relpath)
    filename = os.path.basename(relpath)

    if filename == "Info.plist":
        return canonical_info_plist(path)
    if filename == "Assets.car":
        return canonical_assets_car(path)
    if is_macho(path):
        return canonical_macho(path)

    with open(path, "rb") as f:
        return f.read()


def iter_bundle_files(root):
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = sorted(
            name for name in dirnames if name != "_CodeSignature" and not name.startswith(".")
        )
        for filename in sorted(filenames):
            if filename.startswith(".") or filename in VOLATILE_FILENAMES:
                continue
            path = os.path.join(dirpath, filename)
            relpath = os.path.relpath(path, root)
            yield relpath


def canonical_bundle_lines(root):
    lines = []
    for relpath in iter_bundle_files(root):
        digest = sha256_bytes(canonical_file_bytes(root, relpath))
        lines.append(f"{digest}  {relpath}\n")
    return lines


def canonical_bundle_hash(root):
    lines = canonical_bundle_lines(root)
    return sha256_bytes("".join(lines).encode())


def main():
    manifest = False
    args = sys.argv[1:]
    if args[:1] == ["--manifest"]:
        manifest = True
        args = args[1:]

    if len(args) != 1:
        print("usage: canonical-ios-app-hash.py [--manifest] /path/to/Maple.app", file=sys.stderr)
        return 2

    root = args[0]
    if not os.path.isdir(root):
        print(f"not a directory: {root}", file=sys.stderr)
        return 1

    if manifest:
        sys.stdout.write("".join(canonical_bundle_lines(root)))
    else:
        print(canonical_bundle_hash(root))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
