#!/usr/bin/env python3
"""Sign and package a prebuilt Agent Windows executable: NSIS installer plus updater archive."""

import argparse
import gzip
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import urllib.request
import zipfile


SCRIPTS = Path(__file__).resolve().parent
COMPONENT = SCRIPTS.parent
SIGNER = COMPONENT.parent.parent / "scripts/ci/windows-artifact-sign.ps1"
WINDOWS_ARCHITECTURES = ("x86_64", "aarch64")
NSIS_VERSION = "3.12"
NSIS_URL = "https://downloads.sourceforge.net/project/nsis/NSIS%203/3.12/nsis-3.12.zip"
NSIS_SHA256 = "56581f90db321581c5381193d796fffcf2d24b2f8fed2160a6c6a3baa67f2c4f"
BUNDLE_ROOT = "maple-agent-windows"
BUNDLE_FORMAT = "maple-agent-windows-bundle-v2"
BUNDLE_FILES = ("LICENSE.txt", "build-info.json", "bundle-info.json", "maple-agent.exe")


def platform_name(arch):
    if arch not in WINDOWS_ARCHITECTURES:
        raise ValueError("unsupported Windows architecture")
    return "windows-" + arch


def module(name):
    spec = importlib.util.spec_from_file_location(name.replace("-", "_"), SCRIPTS / f"{name}.py")
    loaded = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(loaded)
    return loaded


release_info = module("release-info")
pe = module("windows-build-info")


def run(arguments, **options):
    command = arguments if isinstance(arguments, str) else [str(value) for value in arguments]
    return subprocess.run(command, check=True, timeout=options.pop("timeout", 120), **options)


def source_sha():
    return run(["git", "-C", COMPONENT, "rev-parse", "HEAD"], capture_output=True, text=True).stdout.strip()


def sign(file):
    run(["pwsh", "-NoLogo", "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", SIGNER, "-File", file], timeout=300)


def nsis_literal(value):
    value = str(value)
    if "\r" in value or "\n" in value or "\0" in value:
        raise ValueError("Invalid NSIS configuration value")
    return value.replace("$", "$$").replace('"', '$\\"')


def write_nsis_config(path, info, payload, installer, unsigned, build, arch="x86_64"):
    platform_name(arch)
    values = {
        "DISPLAY_NAME": info["display_name"], "BUNDLE_ID": info["bundle_id"],
        "VERSION": info["version"], "NUMERIC_VERSION": info["version"].split("-", 1)[0],
        "PAYLOAD": payload, "INSTALLER": installer,
        "ICON": COMPONENT / "app/packaging" / ("maple-agent-dev.ico" if info["profile"] == "dev" else "maple-agent.ico"),
        "BUILD": build, "GENERATION": info["version"] + "-" + build,
        "NATIVE_ARCHITECTURE": "ARM64" if arch == "aarch64" else "AMD64",
        "MIN_WINDOWS_BUILD": "22000" if arch == "aarch64" else "15063",
        "WINDOWS_REQUIREMENT": "Windows 11 or later on an ARM64 computer" if arch == "aarch64" else "Windows 10 version 1703 (build 15063) or later on an x64 computer",
    }
    if not unsigned:
        values["SIGNER"] = SIGNER
    path.write_text("".join(f'!define {key} "{nsis_literal(value)}"\n' for key, value in values.items()), encoding="utf-8", newline="\n")


def unpack_nsis(archive, destination):
    if hashlib.sha256(archive.read_bytes()).hexdigest() != NSIS_SHA256:
        raise ValueError("NSIS archive does not match the reviewed pin")
    with zipfile.ZipFile(archive) as contents:
        contents.extractall(destination)
    return destination / f"nsis-{NSIS_VERSION}/makensis.exe"


def install_nsis(scratch):
    archive = scratch / "nsis.zip"
    with urllib.request.urlopen(NSIS_URL, timeout=90) as response:
        archive.write_bytes(response.read())
    return unpack_nsis(archive, scratch / "nsis")


def build_number():
    value = os.environ.get("MAPLE_PACKAGE_BUILD_NUMBER", "")
    if not re.fullmatch(r"[1-9][0-9]{0,17}", value):
        raise ValueError("MAPLE_PACKAGE_BUILD_NUMBER must be a positive decimal string")
    return value


def bundle_descriptor(info, build):
    if not isinstance(build, str) or not re.fullmatch(r"[1-9][0-9]{0,17}", build):
        raise ValueError("Windows bundle build must be a positive decimal string")
    return {
        "format": BUNDLE_FORMAT, "product": "maple-agent", "profile": info["profile"],
        "version": info["version"], "build": build, "source_sha": info["source_sha"],
        "bundle_id": info["bundle_id"], "entrypoint": "maple-agent.exe",
    }


def write_bundle(payload, archive_path):
    """The updater archive: the installer's generation files under one root."""
    with archive_path.open("xb") as target:
        with gzip.GzipFile(fileobj=target, mode="wb", filename="", mtime=0) as compressed:
            with tarfile.open(fileobj=compressed, mode="w", format=tarfile.USTAR_FORMAT) as archive:
                entry = tarfile.TarInfo(BUNDLE_ROOT)
                entry.type, entry.mode = tarfile.DIRTYPE, 0o755
                archive.addfile(entry)
                for name in BUNDLE_FILES:
                    file = payload / name
                    entry = tarfile.TarInfo(BUNDLE_ROOT + "/" + name)
                    entry.size, entry.mode = file.stat().st_size, 0o644
                    with file.open("rb") as data:
                        archive.addfile(entry, data)


def package(profile, unsigned, arch="x86_64"):
    platform = platform_name(arch)
    binary = COMPONENT / "target/release/maple-agent.exe"
    info = pe.validated_build_info(profile, binary, source_sha(), arch)
    build = build_number()
    destination = COMPONENT / "dist" / profile
    shutil.rmtree(destination, ignore_errors=True)
    destination.parent.mkdir(exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=f".windows-{profile}-", dir=destination.parent) as temporary:
        scratch = Path(temporary).resolve()
        payload = scratch / "payload"
        output = scratch / "output"
        payload.mkdir()
        output.mkdir()
        shutil.copyfile(binary, payload / "maple-agent.exe")
        shutil.copyfile(COMPONENT / "LICENSE", payload / "LICENSE.txt")
        metadata = json.dumps(info, sort_keys=True, separators=(",", ":")) + "\n"
        (payload / "build-info.json").write_text(metadata, encoding="utf-8", newline="\n")
        (payload / "bundle-info.json").write_text(json.dumps(bundle_descriptor(info, build), sort_keys=True, separators=(",", ":")) + "\n", encoding="utf-8", newline="\n")
        if not unsigned:
            sign(payload / "maple-agent.exe")
        installer = output / release_info.windows_package_name(info, platform)
        configuration = scratch / "package.nsh"
        write_nsis_config(configuration, info, payload, installer, unsigned, build, arch)
        compiler = install_nsis(scratch)
        run([compiler, "/V3", f"/DMAPLE_CONFIG={configuration}", SCRIPTS / "windows-installer.nsi"], timeout=300)
        if not unsigned:
            sign(installer)
        write_bundle(payload, output / release_info.windows_archive_name(info, platform))
        shutil.copyfile(payload / "build-info.json", output / "build-info.json")
        release_info.write_manifest(info, output, platform, unsigned, build)
        output.rename(destination)
    print(destination)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("operation", choices=("package",))
    parser.add_argument("profile", choices=("dev", "prod"))
    parser.add_argument("--unsigned", action="store_true")
    parser.add_argument("--arch", choices=WINDOWS_ARCHITECTURES, default="x86_64")
    args = parser.parse_args()
    if sys.platform != "win32":
        raise ValueError("Windows installer packaging requires native Windows")
    package(args.profile, args.unsigned, args.arch)


if __name__ == "__main__":
    main()
