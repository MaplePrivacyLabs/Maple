#!/usr/bin/env python3
"""First-install one trusted Linux bundle; never replace an app or touch profile data."""

import argparse
import ctypes
import errno
import fcntl
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import shlex
import shutil
import stat
import sys
import tempfile


FORMAT = "maple-agent-linux-bundle-v1"
FIELDS = {"format", "product", "profile", "version", "build", "source_sha",
          "bundle_id", "entrypoint", "executable"}
PROFILES = {
    "prod": ("maple-agent.app", "maple-agent", "cloud.opensecret.maple.agent", "Maple Work"),
    "dev": ("maple-agent-dev.app", "maple-agent-dev", "cloud.opensecret.maple.agent.dev", "Maple Work Dev"),
}
MAX_ENTRIES = 20_000
MAX_FILE = 2 * 1024**3
MAX_TOTAL = 4 * 1024**3
MAX_PATH = 1024
MAX_DESCRIPTOR = 16 * 1024
# Non-ELF executables. The bundle format v1 compatibility launcher keeps the
# path where releases that bundled glibc expect their loader.
SCRIPT_EXECUTABLES = ("AppRun", "install.sh", "usr/lib/ld-linux-x86-64.so.2")


class InstallError(RuntimeError):
    pass


def need(condition, message):
    if not condition:
        raise InstallError(message)


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        need(key not in result, "duplicate metadata field")
        result[key] = value
    return result


def read_json(path):
    info = path.lstat()
    need(stat.S_ISREG(info.st_mode) and info.st_nlink == 1 and info.st_size <= MAX_DESCRIPTOR,
         "invalid or oversized metadata file")
    with path.open("rb") as source:
        data = source.read(MAX_DESCRIPTOR + 1)
    need(len(data) <= MAX_DESCRIPTOR, "oversized metadata file")
    return json.loads(data, object_pairs_hook=unique_object)


def validate_descriptor(value, profile=None):
    need(isinstance(value, dict) and set(value) == FIELDS
         and all(isinstance(item, str) for item in value.values()), "invalid bundle descriptor fields")
    need(value["format"] == FORMAT and value["product"] == "maple-agent", "wrong bundle product/format")
    selected = value["profile"]
    need(selected in PROFILES and (profile is None or selected == profile), "wrong bundle profile")
    need(value["bundle_id"] == PROFILES[selected][2]
         and value["entrypoint"] == "AppRun" and value["executable"] == "usr/bin/maple-agent",
         "wrong bundle identity or entry point")
    need(re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?", value["version"])
         and re.fullmatch(r"[1-9][0-9]*", value["build"])
         and re.fullmatch(r"[0-9a-f]{40}", value["source_sha"]), "invalid bundle release identity")
    return value


def digest(path):
    result = hashlib.sha256()
    with path.open("rb") as source:
        while block := source.read(1024 * 1024):
            result.update(block)
    return result.hexdigest()


def inventory(root):
    """Validate a user-owned tree without following directory symlinks."""
    need(root.is_dir() and not root.is_symlink(), "bundle must be a real directory")
    result, total = {}, 0
    for path in (root, *sorted(root.rglob("*"))):
        info = path.lstat()
        relative = path.relative_to(root).as_posix()
        need(len(result) < MAX_ENTRIES and len(relative.encode()) <= MAX_PATH,
             "bundle inventory exceeds bounds")
        need(info.st_uid == os.getuid(), "bundle is not owned by the current user")
        mode = stat.S_IMODE(info.st_mode)
        if stat.S_ISDIR(info.st_mode):
            need(mode == 0o755, "bundle directory has unsafe permissions")
            record = ("directory", mode)
        elif stat.S_ISREG(info.st_mode):
            need(info.st_nlink == 1 and info.st_size <= MAX_FILE, "bundle file is unsafe or oversized")
            total += info.st_size
            need(total <= MAX_TOTAL, "expanded bundle exceeds bounds")
            with path.open("rb") as source:
                executable = source.read(4) == b"\x7fELF"
            executable = executable or relative in SCRIPT_EXECUTABLES
            need(mode == (0o755 if executable else 0o644), "bundle file has unsafe permissions")
            record = ("file", mode, info.st_size, digest(path))
        elif stat.S_ISLNK(info.st_mode):
            target = path.readlink()
            need(not target.is_absolute() and len(str(target).encode()) <= MAX_PATH,
                 "bundle symlink target is invalid")
            try:
                resolved = path.resolve(strict=True)
                resolved.relative_to(root.resolve())
            except (OSError, RuntimeError, ValueError) as error:
                raise InstallError("bundle symlink escapes, cycles or is dangling") from error
            record = ("symlink", str(target))
        else:
            raise InstallError("bundle contains a special file")
        result[relative] = record
    return result


def validate_bundle(root, profile=None):
    descriptor = validate_descriptor(read_json(root / "bundle.json"), profile)
    need(root.name == PROFILES[descriptor["profile"]][0], "unexpected bundle root name")
    records = inventory(root)
    for relative in ("AppRun", "usr/bin/maple-agent", "usr/lib/ld-linux-x86-64.so.2",
                     "install.sh", "usr/share/maple-agent/install.py"):
        need(relative in records and records[relative][0] == "file", "bundle is missing a required regular file")
    return descriptor, records


def owned_directory(path):
    if not path.exists() and not path.is_symlink():
        path.mkdir(mode=0o755)
    info = path.lstat()
    need(stat.S_ISDIR(info.st_mode) and info.st_uid == os.getuid(), "managed parent is not an owned directory")


def owned_parents(home, relative):
    current = home
    need(current.is_dir() and current.stat().st_uid == os.getuid(), "home is not owned by the current user")
    for part in PurePosixPath(relative).parts:
        current /= part
        owned_directory(current)
    return current


def layout(home, profile):
    root_name, command, bundle_id, _ = PROFILES[profile]
    return {
        "bundle": home / ".local" / root_name,
        "wrapper": home / ".local/bin" / command,
        "desktop": home / ".local/share/applications" / (bundle_id + ".desktop"),
        "icon": home / ".local/share/icons/hicolor/256x256/apps" / (bundle_id + ".png"),
        "receipt": home / ".local/share/maple-agent-installer" / (profile + ".json"),
    }


def rename_absent(source, destination):
    """Linux first install must never replace a concurrently created destination."""
    libc = ctypes.CDLL(None, use_errno=True)
    rename = getattr(libc, "renameat2", None)
    need(rename is not None, "safe installation requires Linux renameat2")
    rename.argtypes = (ctypes.c_int, ctypes.c_char_p, ctypes.c_int, ctypes.c_char_p, ctypes.c_uint)
    rename.restype = ctypes.c_int
    if rename(-100, os.fsencode(source), -100, os.fsencode(destination), 1) != 0:
        code = ctypes.get_errno()
        if code == errno.EEXIST:
            raise InstallError("installation already exists; use the app's updater")
        raise OSError(code, "could not publish the first installation")


def exclusive_write(path, data, mode):
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, mode)
    try:
        with os.fdopen(fd, "wb") as target:
            target.write(data)
            target.flush()
            os.fchmod(target.fileno(), mode)
            os.fsync(target.fileno())
    except BaseException:
        path.unlink()
        raise


def desktop_argument(value):
    # Desktop Entry Exec quoting is distinct from shell quoting; percent is a field code.
    value = value.replace("%", "%%")
    return '"' + re.sub(r'([\\"`$])', r'\\\1', value) + '"'


def install(source, home):
    descriptor, expected = validate_bundle(source)
    profile = descriptor["profile"]
    paths = layout(home, profile)
    for relative in (".local", ".local/bin", ".local/share/applications",
                     ".local/share/icons/hicolor/256x256/apps", ".local/share/maple-agent-installer"):
        owned_parents(home, relative)
    need(not any(path.exists() or path.is_symlink() for path in paths.values()),
         "installation or integration path already exists; nothing was replaced")
    bundle_id, display = PROFILES[profile][2:]
    wrapper = ("#!/bin/sh\n# maple-agent-user-install-v1\nexec "
               + shlex.quote(str(paths["bundle"] / "AppRun")) + ' "$@"\n').encode()
    desktop = ("[Desktop Entry]\nType=Application\n" + f"Name={display}\n"
               + "Exec=" + desktop_argument(str(paths["wrapper"])) + " %U\n"
               + f"Icon={bundle_id}\nTerminal=false\nCategories=Office;Utility;\n"
               + f"StartupWMClass={bundle_id}\n").encode()
    icon = source / "usr/share/icons/hicolor/256x256/apps" / (bundle_id + ".png")
    need(icon.is_file() and not icon.is_symlink() and icon.stat().st_size <= 4 * 1024 * 1024,
         "bundle icon is invalid")
    outputs = {"wrapper": (wrapper, 0o755), "desktop": (desktop, 0o644), "icon": (icon.read_bytes(), 0o644)}
    created, installed_identity = [], None
    try:
        with tempfile.TemporaryDirectory(prefix=".maple-agent-install-", dir=home / ".local") as temporary:
            staged = Path(temporary) / source.name
            shutil.copytree(source, staged, symlinks=True)
            need(validate_bundle(staged)[1] == expected, "bundle changed while copying")
            rename_absent(staged, paths["bundle"])
            info = paths["bundle"].stat()
            installed_identity = (info.st_dev, info.st_ino)
            for kind, (data, mode) in outputs.items():
                exclusive_write(paths[kind], data, mode)
                created.append((paths[kind], hashlib.sha256(data).hexdigest()))
            receipt = {"format": "maple-agent-user-install-v1", "profile": profile, "bundle_id": bundle_id,
                       "files": {kind: hashlib.sha256(data).hexdigest() for kind, (data, _) in outputs.items()}}
            exclusive_write(paths["receipt"], (json.dumps(receipt, sort_keys=True) + "\n").encode(), 0o600)
    except BaseException:
        for path, checksum in reversed(created):
            if path.is_file() and not path.is_symlink() and digest(path) == checksum:
                path.unlink()
        if installed_identity and paths["bundle"].is_dir() and not paths["bundle"].is_symlink():
            info = paths["bundle"].stat()
            if (info.st_dev, info.st_ino) == installed_identity:
                shutil.rmtree(paths["bundle"])
        raise
    return paths


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bundle", type=Path, required=True)
    args = parser.parse_args()
    need(sys.platform == "linux" and os.geteuid() != 0, "run as your normal Linux user, without sudo")
    source = args.bundle.resolve(strict=True)
    descriptor = validate_descriptor(read_json(source / "bundle.json"))
    home = Path.home().resolve(strict=True)
    prefix = owned_parents(home, ".local")
    lock_path = prefix / ".maple-agent-install.lock"
    fd = os.open(lock_path, os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)
    with os.fdopen(fd, "rb") as lock:
        info = os.fstat(lock.fileno())
        need(stat.S_ISREG(info.st_mode) and info.st_uid == os.getuid() and info.st_nlink == 1,
             "unsafe installer lock")
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        paths = install(source, home)
        display = PROFILES[descriptor["profile"]][3]
        print("Installed " + display + ". Open your application menu and search for " + display + ".")
        print("Or launch from a terminal: " + shlex.quote(str(paths["wrapper"])))
        print("Updates are managed by the app. Profile data was not changed.")


if __name__ == "__main__":
    try:
        main()
    except (InstallError, OSError, ValueError, KeyError) as error:
        print("Maple installation failed: " + str(error), file=sys.stderr)
        raise SystemExit(1)
