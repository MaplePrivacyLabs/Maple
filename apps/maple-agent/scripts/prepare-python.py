#!/usr/bin/env python3
"""Prepare pinned build-time Python resources. Never used by product startup."""

import argparse
import contextlib
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import platform
import shutil
import sys
import tarfile
import tempfile
import urllib.request

REPO = Path(__file__).resolve().parent.parent
PINS = Path(__file__).with_name("python-runtime.json")
RECEIPT = ".prepared.json"
LICENSES = Path(__file__).with_name("python-licenses")


def digest(path):
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def host_target():
    key = (platform.system(), platform.machine().lower())
    targets = {
        ("Darwin", "arm64"): "aarch64-apple-darwin",
        ("Darwin", "x86_64"): "x86_64-apple-darwin",
        ("Linux", "x86_64"): "x86_64-unknown-linux-gnu",
        ("Windows", "amd64"): "x86_64-pc-windows-msvc",
    }
    if key not in targets:
        raise ValueError(f"No PBS delivery for {key}; Linux ARM64 must use the explicit Nix fixture")
    return targets[key]


@contextlib.contextmanager
def preparation_lock(path):
    """Kernel-owned lock is automatically released after a failed preparation."""
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a+b") as handle:
        if os.name == "nt":
            import msvcrt

            handle.write(b"\0")
            handle.flush()
            handle.seek(0)
            msvcrt.locking(handle.fileno(), msvcrt.LK_LOCK, 1)
        else:
            import fcntl

            fcntl.flock(handle.fileno(), fcntl.LOCK_EX)
        try:
            yield
        finally:
            if os.name == "nt":
                handle.seek(0)
                msvcrt.locking(handle.fileno(), msvcrt.LK_UNLCK, 1)
            else:
                fcntl.flock(handle.fileno(), fcntl.LOCK_UN)


def download(asset, cache, offline):
    archive = cache / (asset["sha256"] + ".tar.gz")
    cache.mkdir(parents=True, exist_ok=True)
    if archive.is_file() and digest(archive) == asset["sha256"]:
        return archive
    if offline:
        raise ValueError(f"Verified Python archive missing from {cache}; run just python-prepare online first")
    # The digest, not an existing filename or an HTTP response, authorizes extraction.
    with tempfile.NamedTemporaryFile(dir=cache, suffix=".download", delete=False) as temporary:
        temporary_path = Path(temporary.name)
        try:
            print(f"Downloading {asset['url']}", file=sys.stderr)
            with urllib.request.urlopen(asset["url"], timeout=60) as response:
                shutil.copyfileobj(response, temporary)
            temporary.flush()
            if digest(temporary_path) != asset["sha256"]:
                raise ValueError("Python archive SHA-256 does not match scripts/python-runtime.json")
        except BaseException:
            temporary.close()
            temporary_path.unlink(missing_ok=True)
            raise
    os.replace(temporary_path, archive)
    return archive


def safe_extract(archive, destination):
    """Keep the archive's python/ root; reject traversal, external links and devices."""
    with tarfile.open(archive, "r:gz") as source:
        members = source.getmembers()
        if len(members) > 50000 or sum(member.size for member in members) > 1024**3:
            raise ValueError("Python archive exceeds extraction limits")
        for member in members:
            name = PurePosixPath(member.name)
            if (
                name.is_absolute()
                or not name.parts
                or name.parts[0] != "python"
                or ".." in name.parts
                or "\\" in member.name
                or ":" in member.name
            ):
                raise ValueError(f"Unsafe Python archive path: {member.name!r}")
            if not (member.isfile() or member.isdir() or member.issym() or member.islnk()):
                raise ValueError(f"Unsupported Python archive entry: {member.name!r}")
            if member.issym() or member.islnk():
                link = PurePosixPath(member.linkname)
                if link.is_absolute() or "\\" in member.linkname or ":" in member.linkname:
                    raise ValueError(f"Unsafe Python archive link: {member.name!r}")
                target = (destination / (name.parent if member.issym() else Path()) / link).resolve()
                if not target.is_relative_to((destination / "python").resolve()):
                    raise ValueError(f"Python archive link escapes its root: {member.name!r}")
        # data_filter additionally resolves symlink chains against the extraction
        # destination and strips unsafe permissions. Requires build Python >=3.13.
        source.extractall(destination, members=members, filter="data")
    return destination / "python"


def inventory(root):
    files = {}
    for path in sorted(root.rglob("*")):
        name = path.relative_to(root).as_posix()
        if name == RECEIPT:
            continue
        if path.is_symlink():
            if not path.resolve().is_relative_to(root.resolve()):
                raise ValueError(f"Staged Python link escapes its root: {name}")
            files[name] = {"link": os.readlink(path)}
        elif path.is_file():
            files[name] = {"sha256": digest(path), "executable": bool(path.stat().st_mode & 0o111)}
    return files


def reusable(root, identity):
    try:
        receipt = json.loads((root / RECEIPT).read_text())
        return receipt["identity"] == identity and receipt["files"] == inventory(root)
    except (OSError, ValueError, KeyError):
        return False


def replace_directory(source, destination):
    """Publish complete staging by rename, retaining the old tree on failure."""
    backup = destination.with_name(destination.name + ".previous")
    if backup.exists():
        if not destination.exists():
            backup.rename(destination)
        else:
            shutil.rmtree(backup)
    had_previous = destination.exists()
    if had_previous:
        destination.rename(backup)
    try:
        source.rename(destination)
    except BaseException:
        if had_previous:
            backup.rename(destination)
        raise
    if had_previous:
        shutil.rmtree(backup)


def prepare_pbs(target, destination, cache, worker, offline=False):
    pins = json.loads(PINS.read_text())
    if target not in pins["targets"]:
        raise ValueError(f"No pinned Python distribution for {target}")
    asset = pins["targets"][target]
    manifest = {
        "protocol_version": 1,
        "implementation": pins["implementation"],
        "version": pins["version"],
        "distribution": f"pbs-{pins['release']}-{target}",
        "executable": asset["executable"],
        "worker": "worker.py",
    }
    identity = {"manifest": manifest, "archive_sha256": asset["sha256"], "worker_sha256": digest(worker), "licenses": inventory(LICENSES)}
    destination.parent.mkdir(parents=True, exist_ok=True)
    with preparation_lock(destination.with_name(destination.name + ".lock")):
        if reusable(destination, identity):
            print(f"Reusing verified Python runtime: {destination}")
            return destination / "runtime.json"
        archive = download(asset, cache, offline)
        with tempfile.TemporaryDirectory(prefix=".python-stage-", dir=destination.parent) as temporary:
            runtime = safe_extract(archive, Path(temporary))
            if not (runtime / asset["executable"]).is_file():
                raise ValueError(f"Python archive lacks {asset['executable']}")
            if not list(runtime.rglob("*LICENSE*")) and not list(runtime.rglob("*license*")):
                raise ValueError("Python archive lacks license notices")
            # install_only excludes upstream's top-level dependency notices.
            # Ship the pinned release's complete notice set alongside notices
            # already present in the Python installation.
            shutil.copytree(LICENSES, runtime / "licenses" / "python-build-standalone", dirs_exist_ok=True)
            shutil.copy2(worker, runtime / "worker.py")
            (runtime / "runtime.json").write_text(json.dumps(manifest, indent=2) + "\n")
            (runtime / RECEIPT).write_text(json.dumps({"identity": identity, "files": inventory(runtime)}, indent=2) + "\n")
            replace_directory(runtime, destination)
    print(f"Prepared Python runtime: {destination}")
    return destination / "runtime.json"


def verify_nix(manifest_path):
    if not manifest_path or not Path(manifest_path).is_absolute():
        raise ValueError("Nix fixture requires explicit MAPLE_CODE_MODE_RUNTIME_MANIFEST from nix develop")
    path = Path(manifest_path)
    manifest = json.loads(path.read_text())
    pins = json.loads(PINS.read_text())
    if (
        manifest.get("protocol_version") != 1
        or manifest.get("implementation") != "cpython"
        or manifest.get("version") != pins["version"]
        or not manifest.get("distribution", "").startswith("nix-")
        or not manifest.get("executable", "").startswith("/nix/store/")
        or not Path(manifest["executable"]).is_file()
        or not (path.parent / manifest["worker"]).is_file()
        or not (path.parent / "licenses" / "Python-LICENSE.txt").is_file()
    ):
        raise ValueError(f"Invalid declared Nix Python installation: {path}")
    print(f"Using declared Nix Python runtime: {path}")
    return path


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--distribution", choices=["pbs", "nix"], default=os.environ.get("MAPLE_CODE_MODE_DISTRIBUTION", "pbs"))
    parser.add_argument("--target")
    parser.add_argument("--destination", type=Path, default=REPO / "target/debug/runtime/python")
    parser.add_argument("--cache", type=Path, default=REPO / "target/python-cache")
    parser.add_argument("--worker", type=Path, default=REPO / "crates/maple-code-mode/python/worker.py")
    parser.add_argument("--manifest", default=os.environ.get("MAPLE_CODE_MODE_RUNTIME_MANIFEST"))
    parser.add_argument("--offline", action="store_true")
    args = parser.parse_args()
    try:
        if sys.version_info < (3, 13):
            raise ValueError("Preparation requires build Python 3.13; use nix develop -c just python-prepare")
        if args.distribution == "nix":
            verify_nix(args.manifest)
        else:
            prepare_pbs(args.target or host_target(), args.destination.resolve(), args.cache.resolve(), args.worker.resolve(), args.offline)
    except (OSError, ValueError, tarfile.TarError) as error:
        parser.exit(1, f"Python preparation failed: {error}\n")


if __name__ == "__main__":
    main()
