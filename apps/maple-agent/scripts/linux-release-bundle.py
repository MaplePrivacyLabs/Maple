#!/usr/bin/env python3
"""Package the audited Linux AppDir as a portable, user-installable tar bundle."""

import argparse
import gzip
import importlib.util
import json
import os
from pathlib import Path, PurePosixPath
import posixpath
import re
import shutil
import sys
import tarfile
import tempfile


SCRIPTS = Path(__file__).resolve().parent


def load(name, filename):
    spec = importlib.util.spec_from_file_location(name, SCRIPTS / filename)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


stage = load("maple_linux_stage", "linux-release-appimage.py")
install = load("maple_linux_install", "linux-install.py")
MAX_EXTENSION = 16 * 1024


def bundle_apprun():
    # A persistent tar installation must never re-enter an inherited outer
    # AppImage, even when that image set APPDIR to this same directory.
    legacy = 'if [ "${APPDIR:-}" != "$appdir" ]; then\n    unset APPIMAGE APPIMAGE_EXTRACT_AND_RUN\nfi\n'
    install.need(stage.APPRUN.count(legacy) == 1, "shared launcher contract changed")
    return stage.APPRUN.replace(legacy, "unset APPIMAGE APPIMAGE_EXTRACT_AND_RUN\n", 1)


def descriptor(metadata, source_sha):
    return install.validate_descriptor({
        "format": install.FORMAT, "product": "maple-agent", "profile": metadata["channel"],
        "version": metadata["version"], "build": metadata["build_number"], "source_sha": source_sha,
        "bundle_id": metadata["bundle_id"], "entrypoint": "AppRun", "executable": "usr/bin/maple-agent",
    })


def _skip(stream, size):
    while size:
        data = stream.read(min(size, 1024 * 1024))
        install.need(data, "truncated tar payload")
        size -= len(data)


def preflight_headers(archive):
    """Bound raw extension sizes before Python's tar parser can allocate them."""
    install.need(archive.is_file() and not archive.is_symlink(), "archive must be a regular file")
    total, count = 0, 0
    with gzip.open(archive, "rb") as stream:
        while True:
            header = stream.read(512)
            install.need(len(header) == 512, "tar has no complete end marker")
            if header == bytes(512):
                install.need(stream.read(512) == bytes(512), "invalid tar end marker")
                trailing = 0
                while block := stream.read(1024):
                    trailing += len(block)
                    install.need(trailing <= 10240 and not any(block), "unexpected trailing tar content")
                return
            count += 1
            install.need(count <= install.MAX_ENTRIES, "too many tar headers")
            member = tarfile.TarInfo.frombuf(header, "utf-8", "strict")
            install.need(member.size >= 0, "negative tar size")
            if member.type in (tarfile.GNUTYPE_LONGNAME, tarfile.GNUTYPE_LONGLINK,
                               tarfile.XHDTYPE, tarfile.XGLTYPE):
                install.need(member.size <= MAX_EXTENSION, "oversized tar extension")
            else:
                install.need(member.isfile() or member.isdir() or member.issym(), "unsupported tar entry type")
                install.need(member.size <= install.MAX_FILE, "oversized tar file")
                total += member.size
                install.need(total <= install.MAX_TOTAL, "expanded tar exceeds bounds")
            _skip(stream, (member.size + 511) // 512 * 512)


def _safe_name(name):
    install.need(isinstance(name, str) and name and len(name.encode()) <= install.MAX_PATH
                 and not any(ord(char) < 32 or char == "\\" for char in name), "invalid tar path")
    path = PurePosixPath(name)
    install.need(not path.is_absolute() and ".." not in path.parts and str(path) == name,
                 "non-canonical or escaping tar path")
    return path


def inspect_archive(archive, expected_profile=None):
    archive = Path(archive)
    preflight_headers(archive)
    members, links, value = {}, {}, None
    with tarfile.open(archive, "r:gz") as source:
        for member in source:
            name = member.name.rstrip("/") if member.isdir() else member.name
            path = _safe_name(name)
            install.need(name not in members and len(members) < install.MAX_ENTRIES,
                         "duplicate or excessive tar entries")
            install.need(member.sparse is None and not any("sparse" in key.lower() for key in member.pax_headers),
                         "sparse archives are unsupported")
            install.need(member.isfile() or member.isdir() or member.issym(), "unsupported tar entry")
            install.need(0 <= member.size <= install.MAX_FILE, "oversized tar member")
            if member.isdir():
                install.need(member.mode == 0o755, "unsafe tar directory permissions")
            elif member.isfile():
                payload = source.extractfile(member)
                prefix = payload.read(4)
                relative = PurePosixPath(*path.parts[1:]).as_posix() if len(path.parts) > 1 else ""
                executable = prefix == b"\x7fELF" or relative in install.SCRIPT_EXECUTABLES
                install.need(member.mode == (0o755 if executable else 0o644), "unsafe tar file permissions")
                if path.name == "bundle.json" and len(path.parts) == 2:
                    install.need(member.size <= install.MAX_DESCRIPTOR, "oversized bundle descriptor")
                    data = prefix + payload.read(install.MAX_DESCRIPTOR + 1)
                    install.need(len(data) == member.size, "truncated bundle descriptor")
                    value = install.validate_descriptor(json.loads(data, object_pairs_hook=install.unique_object), expected_profile)
                payload.close()
            else:
                install.need(member.size == 0 and member.linkname and not member.linkname.startswith("/")
                             and len(member.linkname.encode()) <= install.MAX_PATH
                             and not any(ord(char) < 32 or char == "\\" for char in member.linkname),
                             "invalid tar symlink")
                target = posixpath.normpath(str(path.parent / member.linkname))
                target_path = _safe_name(target)
                install.need(target_path.parts[0] == path.parts[0], "tar symlink escapes root")
                links[name] = target
            members[name] = member
    install.need(value is not None, "missing root bundle descriptor")
    root = install.PROFILES[value["profile"]][0]
    install.need(root in members and members[root].isdir()
                 and all(PurePosixPath(name).parts[0] == root for name in members), "wrong or multiple bundle roots")
    total = sum(member.size for member in members.values() if member.isfile())
    install.need(total <= install.MAX_TOTAL, "expanded tar exceeds bounds")
    for name in members:
        install.need(not any(str(parent) in links for parent in PurePosixPath(name).parents),
                     "tar writes through a symlink")
    for name, target in links.items():
        visited = {name}
        while target in links:
            install.need(target not in visited and len(visited) < 40, "tar symlink cycle")
            visited.add(target)
            target = links[target]
        install.need(target in members, "tar contains dangling symlink")
    for relative in ("bundle.json", "AppRun", "usr/bin/maple-agent", "usr/lib/ld-linux-x86-64.so.2",
                     "install.sh", "usr/share/maple-agent/install.py"):
        member = members.get(root + "/" + relative)
        install.need(member is not None and member.isfile(), "tar is missing required regular payload")
    return value, members


def extract_archive(archive, output, profile=None):
    value, members = inspect_archive(archive, profile)
    output = Path(output)
    install.need(not output.exists() and not output.is_symlink(), "extraction output must be absent")
    output.mkdir(mode=0o700, parents=True)
    with tarfile.open(archive, "r:gz") as source:
        for name, member in sorted(members.items(), key=lambda item: (len(PurePosixPath(item[0]).parts), item[0])):
            if member.issym():
                continue
            path = output / name
            if member.isdir():
                path.mkdir(mode=0o755)
                path.chmod(0o755)
            else:
                with source.extractfile(member) as incoming, path.open("xb") as target:
                    shutil.copyfileobj(incoming, target, 1024 * 1024)
                path.chmod(member.mode)
        for name, member in members.items():
            if member.issym():
                (output / name).symlink_to(member.linkname)
    return output / install.PROFILES[value["profile"]][0]


def audit(root):
    value, _ = install.validate_bundle(Path(root))
    actual = stage.audit(Path(root))
    recorded = install.read_json(Path(root) / "usr/share/maple-agent/runtime-audit.json")
    install.need(actual == recorded, "bundle runtime audit differs")
    return value


def build(binary, output):
    binary, output = Path(binary).absolute(), Path(output).absolute()
    install.need(not output.exists() and not output.is_symlink(), "refusing to replace an existing artifact")
    metadata = stage.metadata_from_environment()
    value = descriptor(metadata, os.environ["MAPLE_PACKAGE_SOURCE_SHA"])
    prefix = f"maple-agent-{value['profile']}-{value['version']}-"
    suffix = "-linux-x86_64.tar.gz"
    install.need(output.name.startswith(prefix) and output.name.endswith(suffix), "wrong Linux artifact name")
    revision = output.name[len(prefix):-len(suffix)]
    install.need(re.fullmatch(r"[0-9a-f]{7,40}", revision) and value["source_sha"].startswith(revision),
                 "artifact revision differs from bundle source")
    epoch = int(os.environ["SOURCE_DATE_EPOCH"])
    install.need(0 <= epoch <= 2**32 - 1, "invalid source date epoch")
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=".linux-bundle-", dir=output.parent) as temporary:
        temporary = Path(temporary)
        root = temporary / install.PROFILES[value["profile"]][0]
        stage.run(sys.executable, SCRIPTS / "linux-release-appimage.py", "--stage-only", binary, root)
        (root / "AppRun").write_text(bundle_apprun())
        shutil.copyfile(SCRIPTS / "linux-install.sh", root / "install.sh")
        shutil.copyfile(SCRIPTS / "linux-bundle-README.txt", root / "README.txt")
        shutil.copyfile(SCRIPTS / "linux-install.py", root / "usr/share/maple-agent/install.py")
        (root / "bundle.json").write_text(json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n")
        stage.normalize_public_permissions(root)
        audit(root)
        packed = temporary / "payload.tar.gz"
        with packed.open("xb") as target, gzip.GzipFile(filename="", mode="wb", fileobj=target, mtime=epoch) as compressed:
            with tarfile.open(fileobj=compressed, mode="w", format=tarfile.GNU_FORMAT, dereference=False) as archive:
                for path in (root, *sorted(root.rglob("*"))):
                    name = root.name + ("/" + path.relative_to(root).as_posix() if path != root else "")
                    member = archive.gettarinfo(str(path), name)
                    member.uid = member.gid = 0
                    member.uname = member.gname = ""
                    member.mtime = epoch
                    with path.open("rb") if member.isfile() else open(os.devnull, "rb") as incoming:
                        archive.addfile(member, incoming if member.isfile() else None)
        packed.chmod(0o644)
        inspect_archive(packed, value["profile"])
        os.link(packed, output)
    return output


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("operation", help="BINARY, audit or extract")
    parser.add_argument("path", type=Path)
    parser.add_argument("output", type=Path, nargs="?")
    parser.add_argument("--profile", choices=("dev", "prod"))
    args = parser.parse_args()
    if args.operation == "audit":
        install.need(args.output is None, "audit accepts one bundle directory")
        print(json.dumps(audit(args.path), sort_keys=True))
    elif args.operation == "extract":
        install.need(args.output is not None, "extract requires archive and absent output directory")
        print(extract_archive(args.path, args.output, args.profile))
    else:
        install.need(args.output is None and args.profile is None, "build accepts binary and output archive")
        print(build(Path(args.operation), args.path))


if __name__ == "__main__":
    try:
        main()
    except (install.InstallError, stage.PackagingError, OSError, ValueError, KeyError, tarfile.TarError) as error:
        print("Maple Linux bundle failed: " + str(error), file=sys.stderr)
        raise SystemExit(1)
