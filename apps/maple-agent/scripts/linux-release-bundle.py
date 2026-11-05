#!/usr/bin/env python3
"""Package the staged Linux AppDir as a portable, user-installable tar bundle."""

import argparse
import gzip
import importlib.util
import json
import os
from pathlib import Path
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
        os.link(packed, output)
    return output


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    print(build(args.binary, args.output))


if __name__ == "__main__":
    try:
        main()
    except (install.InstallError, stage.PackagingError, OSError, ValueError, KeyError, tarfile.TarError) as error:
        print("Maple Linux bundle failed: " + str(error), file=sys.stderr)
        raise SystemExit(1)
