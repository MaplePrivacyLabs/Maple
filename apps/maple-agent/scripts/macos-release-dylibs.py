#!/usr/bin/env python3
"""Relocate Agent's non-system Mach-O dylib closure before Swift scan/signing."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys


class PackagingError(RuntimeError):
    pass


def native(tool, *args):
    result = subprocess.run(
        [f"/usr/bin/{tool}", *map(str, args)], text=True, capture_output=True
    )
    if result.returncode:
        raise PackagingError(f"{tool} failed: {result.stderr.strip()}")
    return result.stdout


def digest(path):
    value = hashlib.sha256()
    with path.open("rb") as stream:
        while chunk := stream.read(1024 * 1024):
            value.update(chunk)
    return value.hexdigest()


def inspect(path):
    dependencies = []
    for line in native("otool", "-L", path).splitlines():
        match = re.match(r"^\s+(.+?) \(compatibility version ", line)
        if match and match[1] not in dependencies:
            dependencies.append(match[1])
    identifiers = [
        line.strip() for line in native("otool", "-D", path).splitlines()
        if line.strip() and not line.rstrip().endswith(":")
    ]
    # Fat images may report the same install ID for more than one slice.
    identifiers = list(dict.fromkeys(identifiers))
    if len(identifiers) > 1:
        raise PackagingError(f"Mach-O slices have inconsistent install IDs: {path}")
    identifier = identifiers[0] if identifiers else None
    dependencies = [value for value in dependencies if value != identifier]
    rpaths = []
    is_rpath = False
    for line in native("otool", "-l", path).splitlines():
        if line.strip().startswith("cmd "):
            is_rpath = line.strip() == "cmd LC_RPATH"
        elif is_rpath:
            match = re.match(r"\s*path (.+?) \(offset \d+\)", line)
            if match:
                if match[1] not in rpaths:
                    rpaths.append(match[1])
                is_rpath = False
    return dependencies, rpaths, identifier


def system(name):
    normalized = os.path.normpath(name)
    return normalized.startswith(("/usr/lib/", "/System/Library/"))


def swift_rpath(name):
    return bool(re.fullmatch(r"@rpath/libswift[A-Za-z0-9_]+\.dylib", name))


def expand(name, owner, executable):
    if name.startswith("@loader_path/"):
        return owner.parent / name[len("@loader_path/"):]
    if name.startswith("@executable_path/"):
        return executable.parent / name[len("@executable_path/"):]
    if name.startswith("/"):
        return Path(name)
    raise PackagingError(f"Unsupported runtime search path: {name}")


class Relocator:
    def __init__(self, source_binary, app):
        self.executable = source_binary.resolve()
        self.app = app.resolve()
        self.binary = self.app / "Contents/MacOS/maple-agent"
        self.frameworks = self.app / "Contents/Frameworks"
        if not self.executable.is_file() or not self.binary.is_file():
            raise PackagingError("Original and staged Agent executables must exist")
        if self.binary.is_symlink() or os.path.samefile(self.executable, self.binary):
            raise PackagingError("Staged executable must be a separate copy of the source")
        for directory in (self.binary.parent, self.frameworks):
            if directory.is_symlink() or not directory.is_dir():
                raise PackagingError("Bundle binary/Frameworks directories must exist without symlinks")
            try:
                directory.resolve().relative_to(self.app)
            except ValueError:
                raise PackagingError("Bundle directories escape the staged application")
        if digest(self.executable) != digest(self.binary):
            raise PackagingError("Staged executable differs from the original before relocation")
        self.pending = [(self.executable, self.binary, [])]
        self.by_source = {}
        self.by_name = {}
        self.public_libraries = []

    def resolve(self, dependency, source, search_paths):
        if dependency.startswith("@rpath/"):
            suffix = dependency[len("@rpath/"):]
            candidates = [expand(path, owner, self.executable) / suffix for path, owner in search_paths]
        else:
            candidates = [expand(dependency, source, self.executable)]
        for candidate in candidates:
            if candidate.is_file():
                return candidate.resolve()
        raise PackagingError(f"Cannot resolve dylib {dependency} referred to by {source.name}")

    def stage_library(self, source, search_paths):
        if source in self.by_source:
            return self.by_source[source]
        name = source.name
        if not re.fullmatch(r"[A-Za-z0-9_.+\-]+\.dylib", name):
            raise PackagingError(f"Unsupported non-system framework or dylib name: {name}")
        source_hash = digest(source)
        if name in self.by_name:
            target, previous_hash = self.by_name[name]
            if previous_hash != source_hash:
                raise PackagingError(f"Different dylibs share package basename: {name}")
            self.by_source[source] = target
            return target
        target = self.frameworks / name
        if target.exists() or target.is_symlink():
            raise PackagingError(f"Refusing to replace preexisting Frameworks library: {name}")
        shutil.copy2(source, target)
        target.chmod(0o755)
        self.by_source[source] = target
        self.by_name[name] = (target, source_hash)
        self.public_libraries.append({"name": name, "source_sha256": source_hash})
        self.pending.append((source, target, search_paths))
        return target

    def relocate(self):
        cursor = 0
        while cursor < len(self.pending):
            source, target, inherited_paths = self.pending[cursor]
            cursor += 1
            dependencies, rpaths, identifier = inspect(source)
            search_paths = [(value, source) for value in rpaths] + inherited_paths
            for dependency in dependencies:
                if system(dependency) or swift_rpath(dependency):
                    continue
                resolved = self.resolve(dependency, source, search_paths)
                staged = self.stage_library(resolved, search_paths)
                native("install_name_tool", "-change", dependency, f"@rpath/{staged.name}", target)
            if target != self.binary:
                if not identifier:
                    raise PackagingError(f"Resolved non-system dependency is not a dylib: {source.name}")
                native("install_name_tool", "-id", f"@rpath/{target.name}", target)
            # Flattening changes loader-relative search paths. Replace all source
            # paths after using them to resolve the original dependency closure.
            for value in rpaths:
                native("install_name_tool", "-delete_rpath", value, target)
            for value in ("/usr/lib/swift", "@executable_path/../Frameworks" if target == self.binary else "@loader_path"):
                native("install_name_tool", "-add_rpath", value, target)
        self.audit()
        return {"libraries": sorted(self.public_libraries, key=lambda value: value["name"])}

    def audit(self):
        for path in (self.binary, *(entry[0] for entry in self.by_name.values())):
            dependencies, rpaths, identifier = inspect(path)
            expected = ["/usr/lib/swift", "@executable_path/../Frameworks" if path == self.binary else "@loader_path"]
            if rpaths != expected:
                raise PackagingError(f"Unexpected staged runtime search paths: {path.name}")
            if path != self.binary and identifier != f"@rpath/{path.name}":
                raise PackagingError(f"Unexpected staged dylib install ID: {path.name}")
            for dependency in dependencies:
                if system(dependency) or swift_rpath(dependency):
                    continue
                if not dependency.startswith("@rpath/") or "/" in dependency[len("@rpath/"):]:
                    raise PackagingError(f"Nonportable staged dylib dependency: {dependency}")
                if not (self.frameworks / dependency[len("@rpath/"):]).is_file():
                    raise PackagingError(f"Missing staged dylib dependency: {dependency}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source_binary", type=Path)
    parser.add_argument("staged_app", type=Path)
    args = parser.parse_args()
    result = Relocator(args.source_binary, args.staged_app).relocate()
    print(json.dumps(result, sort_keys=True))


if __name__ == "__main__":
    try:
        main()
    except (PackagingError, OSError) as error:
        print(f"macOS dylib relocation failed: {error}", file=sys.stderr)
        sys.exit(1)
