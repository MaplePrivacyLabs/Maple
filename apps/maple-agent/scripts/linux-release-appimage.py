#!/usr/bin/env python3
"""Build Agent's x86_64 AppImage from the owning pinned Nix shell.

The AppRun uses the matching bundled glibc loader, without exporting a library
path into the agent's child CLI processes. Host GPU drivers and desktop service
configuration remain host resources. A separate clean-machine smoke must gate
publication; an ELF closure audit does not prove GUI/audio/CUA integration.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile


INTERPRETER = "/lib64/ld-linux-x86-64.so.2"
LOADER = "ld-linux-x86-64.so.2"
# These are loaded dynamically and therefore absent from DT_NEEDED scans.
DLOPEN_LIBRARIES = (
    "libwayland-client.so.0",
    "libwayland-cursor.so.0",
    "libwayland-egl.so.1",
    "libvulkan.so.1",
    # GPUI enables both Vulkan and OpenGL backends. Vendor drivers remain host
    # resources; libglvnd discovers their normal /etc and /usr/share manifests.
    "libEGL.so.1",
    "libxcb.so.1",
    "libxkbcommon.so.0",
    "libxkbcommon-x11.so.0",
    "libX11.so.6",
    "libX11-xcb.so.1",
    "libXi.so.6",
    "libXtst.so.6",
    # libgcc is dynamically opened by glibc's unwinder.
    "libgcc_s.so.1",
)


class PackagingError(RuntimeError):
    pass


def run(*args, check=True, env=None, umask=-1):
    result = subprocess.run(
        [str(arg) for arg in args], capture_output=True, text=True, env=env, umask=umask
    )
    if check and result.returncode:
        raise PackagingError(
            f"Command failed ({result.returncode}): {args[0]}\n"
            f"{result.stdout}{result.stderr}"
        )
    return result


def elf(path):
    with path.open("rb") as stream:
        header = stream.read(20)
    return header[:4] == b"\x7fELF"


def require_x86_64_elf(path):
    with path.open("rb") as stream:
        header = stream.read(20)
    if header[:6] != b"\x7fELF\x02\x01" or header[18:20] != b"\x3e\x00":
        raise PackagingError(f"Expected x86_64 little-endian ELF: {path}")


def inside(path, root):
    try:
        path.resolve().relative_to(root.resolve())
        return True
    except ValueError:
        return False


def digest(path):
    with path.open("rb") as stream:
        hasher = hashlib.sha256()
        while chunk := stream.read(1024 * 1024):
            hasher.update(chunk)
        return hasher.hexdigest()


def needed(path):
    return run("patchelf", "--print-needed", path).stdout.splitlines()


def interpreter(path):
    result = run("patchelf", "--print-interpreter", path, check=False)
    return result.stdout.strip() if result.returncode == 0 else ""


class Closure:
    def __init__(self, store_paths):
        self.roots = [Path(line) for line in store_paths.read_text().splitlines() if line]
        if not self.roots or any(not root.is_absolute() for root in self.roots):
            raise PackagingError("Runtime closure must list absolute pinned store paths")
        self.index = {}
        for root in self.roots:
            for libdir in (root / "lib", root / "lib64"):
                if not libdir.is_dir():
                    continue
                for path in sorted(libdir.rglob("*")):
                    if path.is_file() and (".so" in path.name or path.name == LOADER):
                        self.index.setdefault(path.name, []).append(path.resolve())

    def approved(self, path):
        return any(inside(path, root) for root in self.roots)

    def library(self, name, requester=None):
        soname = Path(name).name
        if not re.fullmatch(r"[A-Za-z0-9_.+\-]+", soname):
            raise PackagingError(f"Invalid DT_NEEDED name: {name}")
        if "/" in name:
            source = Path(name)
            if not source.is_absolute() or not source.is_file() or not self.approved(source):
                raise PackagingError(f"Dependency is outside the pinned runtime closure: {name}")
            return source.resolve()
        # Preserve the linker's selected version when the closure includes more
        # than one version of a SONAME.
        if requester:
            rpath = run("patchelf", "--print-rpath", requester).stdout.strip()
            for entry in rpath.split(":"):
                if not entry:
                    continue
                entry = entry.replace("${ORIGIN}", str(requester.parent))
                entry = entry.replace("$ORIGIN", str(requester.parent))
                candidate = Path(entry) / soname
                if candidate.is_file() and self.approved(candidate):
                    return candidate.resolve()
        candidates = list(dict.fromkeys(self.index.get(soname, [])))
        if not candidates:
            raise PackagingError(f"Missing pinned runtime library: {soname}")
        if len({digest(candidate) for candidate in candidates}) != 1:
            raise PackagingError(f"Ambiguous pinned runtime library: {soname}")
        return candidates[0]


def stage_closure(appdir, binary, closure, glibc):
    libdir = appdir / "usr/lib"
    libdir.mkdir(parents=True, exist_ok=True)
    destination = appdir / "usr/bin/maple-agent"
    destination.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(binary, destination)
    destination.chmod(0o755)
    pending = [(destination, binary)]
    sources = {}

    def copy_library(source, name):
        target = libdir / name
        if name in sources:
            if digest(sources[name]) != digest(source):
                raise PackagingError(f"Conflicting libraries share SONAME {name}")
            return
        require_x86_64_elf(source)
        shutil.copy2(source, target)
        target.chmod(0o755)
        sources[name] = source
        pending.append((target, source))

    loader = glibc / "lib" / LOADER
    if not loader.is_file() or not closure.approved(loader):
        raise PackagingError("Matching glibc loader is missing from pinned runtime closure")
    copy_library(loader.resolve(), LOADER)
    for soname in DLOPEN_LIBRARIES:
        copy_library(closure.library(soname, binary), soname)
    # glibc can open these modules at runtime according to host nsswitch.conf.
    # Modern glibc folds files/dns into libc; older pins may provide separate DSOs.
    for source in sorted((glibc / "lib").glob("libnss_*.so*")):
        if source.is_file():
            copy_library(source.resolve(), source.name)
    # Keep character-conversion modules paired with the bundled libc. They are
    # deliberately not activated through exported GCONV_PATH, which could make
    # a host CLI child load modules for the wrong libc. Built-in UTF-8 conversion
    # remains available; other codecs need a future process-scoped module path.
    # Do not reuse a build-machine cache containing store-relative module paths.
    gconv = glibc / "lib/gconv"
    if gconv.is_dir():
        for source in sorted(gconv.rglob("*")):
            if not source.is_file() or source.name.endswith(".cache"):
                continue
            target = libdir / "gconv" / source.relative_to(gconv)
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(source, target)
            target.chmod(0o755 if elf(source) else 0o644)
            if elf(source):
                require_x86_64_elf(source)
                pending.append((target, source))
            elif b"/nix/store" in target.read_bytes():
                raise PackagingError(f"Build-store path in gconv module configuration: {source}")
    cursor = 0
    while cursor < len(pending):
        target, source = pending[cursor]
        cursor += 1
        for dependency in needed(source):
            soname = Path(dependency).name
            copy_library(closure.library(dependency, source), soname)
            if soname != dependency:
                run("patchelf", "--replace-needed", dependency, soname, target)
        if target.name != LOADER:
            relative_libdir = os.path.relpath(libdir, target.parent)
            rpath = "$ORIGIN" if relative_libdir == "." else f"$ORIGIN/{relative_libdir}"
            run("patchelf", "--set-rpath", rpath, target)
            if interpreter(target):
                # Raw executable metadata is conventional. AppRun explicitly
                # invokes our matching loader instead of this host interpreter.
                run("patchelf", "--set-interpreter", INTERPRETER, target)
    if "libc.so.6" not in sources:
        raise PackagingError("Release binary has no libc dependency; expected native GPUI build")
    if digest(sources["libc.so.6"]) != digest((glibc / "lib/libc.so.6").resolve()):
        raise PackagingError("Bundled libc and loader come from different glibc builds")


APPRUN = r'''#!/bin/sh
set -eu
appdir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
# Only the main Agent process receives this loader path. External CLI tools
# continue to use their host loader and environment.
library_path="$appdir/usr/lib:/lib/x86_64-linux-gnu:/usr/lib/x86_64-linux-gnu:/lib64:/usr/lib64:/lib:/usr/lib"
if [ -z "${FONTCONFIG_FILE:-}" ]; then
    if [ -f /etc/fonts/fonts.conf ]; then
        export FONTCONFIG_FILE=/etc/fonts/fonts.conf
        export FONTCONFIG_PATH=/etc/fonts
    else
        export FONTCONFIG_FILE="$appdir/usr/share/maple-agent/fonts.conf"
    fi
fi
# ALSA's package defaults point into the build store. Use the desktop's audio
# routing and plugins (including PulseAudio/PipeWire) when they are installed.
if [ -f /usr/share/alsa/alsa.conf ]; then
    export ALSA_CONFIG_PATH="${ALSA_CONFIG_PATH:-/usr/share/alsa/alsa.conf}"
    export ALSA_CONFIG_DIR="${ALSA_CONFIG_DIR:-/usr/share/alsa}"
fi
if [ -d /usr/lib/x86_64-linux-gnu/alsa-lib ]; then
    export ALSA_PLUGIN_DIR="${ALSA_PLUGIN_DIR:-/usr/lib/x86_64-linux-gnu/alsa-lib}"
fi
# The Nix loader cannot use the host ld.so.cache. Select host GPU manifests
# explicitly, and include standard host library directories above for their
# transitive dependencies. Driver libraries themselves are never bundled.
if [ -z "${VK_DRIVER_FILES:-}" ] && [ -z "${VK_ICD_FILENAMES:-}" ]; then
    driver_files=""
    for manifest in /etc/vulkan/icd.d/*.json /usr/share/vulkan/icd.d/*.json; do
        [ -f "$manifest" ] || continue
        driver_files="${driver_files:+$driver_files:}$manifest"
    done
    if [ -n "$driver_files" ]; then
        export VK_DRIVER_FILES="$driver_files"
    fi
fi
exec "$appdir/usr/lib/ld-linux-x86-64.so.2" --inhibit-cache --library-path "$library_path" "$appdir/usr/bin/maple-agent" "$@"
'''

FONTS_CONF = '''<?xml version="1.0"?>
<!DOCTYPE fontconfig SYSTEM "urn:fontconfig:fonts.dtd">
<fontconfig>
  <dir>/usr/share/fonts</dir>
  <dir>/usr/local/share/fonts</dir>
  <dir prefix="xdg">fonts</dir>
  <cachedir prefix="xdg">fontconfig</cachedir>
</fontconfig>
'''


def stage_metadata(appdir, component, metadata):
    icon = component / "app/packaging" / ("maple-agent-dev.png" if metadata["channel"] == "dev" else "maple-agent.png")
    if not icon.is_file():
        raise PackagingError(f"Missing Agent package icon: {icon}")
    desktop = appdir / "usr/share/applications" / f"{metadata['bundle_id']}.desktop"
    desktop.parent.mkdir(parents=True, exist_ok=True)
    desktop.write_text(
        "[Desktop Entry]\nType=Application\n"
        f"Name={metadata['app_name']}\nExec=maple-agent %U\n"
        f"Icon={metadata['bundle_id']}\nTerminal=false\nCategories=Office;Utility;\n"
        f"StartupWMClass={metadata['bundle_id']}\n"
    )
    staged_icon = appdir / "usr/share/icons/hicolor/256x256/apps" / f"{metadata['bundle_id']}.png"
    staged_icon.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(icon, staged_icon)
    (appdir / desktop.name).symlink_to(desktop.relative_to(appdir))
    (appdir / staged_icon.name).symlink_to(staged_icon.relative_to(appdir))
    (appdir / ".DirIcon").symlink_to(staged_icon.name)
    data = appdir / "usr/share/maple-agent"
    data.mkdir(parents=True, exist_ok=True)
    (data / "fonts.conf").write_text(FONTS_CONF)
    (data / "package-metadata.json").write_text(json.dumps(metadata, indent=2) + "\n")
    (appdir / "AppRun").write_text(APPRUN)
    (appdir / "AppRun").chmod(0o755)
    return desktop, staged_icon


def normalize_public_permissions(appdir):
    # The signing/publishing caller uses umask 077. SquashFS stores these modes,
    # so public package payload modes must be explicit for ordinary users.
    appdir.chmod(0o755)
    for path in appdir.rglob("*"):
        if path.is_symlink():
            continue
        if path.is_dir():
            path.chmod(0o755)
        elif path.is_file():
            path.chmod(0o755 if path.name == "AppRun" or elf(path) else 0o644)


def glibc_version_needs(output):
    providers = {}
    in_needs = False
    provider = None
    for line in output.splitlines():
        if line.startswith("Version "):
            in_needs = line.startswith("Version needs section")
            provider = None
        if not in_needs:
            continue
        file_match = re.search(r"\bFile:\s+(\S+)", line)
        if file_match:
            provider = Path(file_match[1]).name
        name_match = re.search(r"\bName:\s+(GLIBC_[A-Za-z0-9_.]+)", line)
        if name_match:
            if provider is None:
                raise PackagingError("GLIBC version need has no provider File: SONAME")
            providers.setdefault(provider, set()).add(name_match[1])
    return providers


def glibc_version_definitions(output):
    supplied = set()
    in_definitions = False
    for line in output.splitlines():
        if line.startswith("Version "):
            in_definitions = line.startswith("Version definition section")
        if in_definitions:
            match = re.search(r"\bName:\s+(GLIBC_[A-Za-z0-9_.]+)", line)
            if match:
                supplied.add(match[1])
    return supplied


def audit(appdir, metadata=None):
    if metadata is None:
        metadata = validate_metadata(json.loads((appdir / "usr/share/maple-agent/package-metadata.json").read_text()))
    libdir = appdir / "usr/lib"
    required = set()
    provider_requirements = []
    elf_count = 0
    for path in (appdir, *sorted(appdir.rglob("*"))):
        if path.is_symlink() and (not path.exists() or not inside(path, appdir)):
            raise PackagingError(f"AppDir symlink escapes package or is dangling: {path}")
        if path.is_dir() and not path.is_symlink() and path.stat().st_mode & 0o777 != 0o755:
            raise PackagingError(f"Restrictive or unsafe public package directory permissions: {path}")
        if path.is_file() and not path.is_symlink():
            expected_mode = 0o755 if path.name == "AppRun" or elf(path) else 0o644
            if path.stat().st_mode & 0o7777 != expected_mode:
                raise PackagingError(f"Restrictive or unsafe public package file permissions: {path}")
        if not path.is_file() or path.is_symlink() or not elf(path):
            continue
        require_x86_64_elf(path)
        elf_count += 1
        for dependency in needed(path):
            if "/" in dependency:
                raise PackagingError(f"Absolute DT_NEEDED remains in {path}: {dependency}")
            if not (libdir / dependency).is_file():
                raise PackagingError(f"Unbundled DT_NEEDED in {path}: {dependency}")
        if path.name != LOADER:
            rpath = run("patchelf", "--print-rpath", path).stdout.strip()
            relative_libdir = os.path.relpath(libdir, path.parent)
            expected = "$ORIGIN" if relative_libdir == "." else f"$ORIGIN/{relative_libdir}"
            if rpath != expected:
                raise PackagingError(f"Non-relative runtime path in {path}: {rpath}")
            if interpreter(path) not in ("", INTERPRETER):
                raise PackagingError(f"Non-system interpreter in {path}")
        versions = run("readelf", "--version-info", path).stdout
        for provider, versions_needed in glibc_version_needs(versions).items():
            required.update(versions_needed)
            provider_requirements.append((provider, versions_needed))
    definitions_by_provider = {}
    for provider, versions_needed in provider_requirements:
        if provider not in definitions_by_provider:
            library = libdir / provider
            if not library.is_file():
                raise PackagingError(f"Missing bundled GLIBC version provider: {provider}")
            definitions_by_provider[provider] = glibc_version_definitions(
                run("readelf", "--version-info", library).stdout
            )
        missing = versions_needed - definitions_by_provider[provider]
        if missing:
            raise PackagingError(f"Bundled {provider} does not supply required versions: {sorted(missing)}")
    for soname in (LOADER, *DLOPEN_LIBRARIES):
        if not (libdir / soname).is_file():
            raise PackagingError(f"Missing dynamic runtime payload: {soname}")
    for path in (appdir / "AppRun", *appdir.glob("*.desktop")):
        if b"/nix/store" in path.read_bytes():
            raise PackagingError(f"Build-store path in launch metadata: {path}")
    audit_metadata = {
        "schema": 1,
        "channel": metadata["channel"],
        "architecture": "x86_64",
        "bundled_glibc": True,
        "gconv_path_exported": False,
        "loader": f"usr/lib/{LOADER}",
        "loader_sha256": digest(libdir / LOADER),
        "libc_sha256": digest(libdir / "libc.so.6"),
        "elf_count": elf_count,
        "required_glibc_versions": sorted(required),
        "host_resources": ["Vulkan GPU drivers", "fontconfig", "ALSA routing/plugins"],
    }
    return audit_metadata


def extract_appimage(image, destination):
    # Do not execute the AppImage runtime to extract pinned tools or final output.
    data = image.read_bytes()
    offset = data.find(b"hsqs")
    while offset >= 0:
        if run("unsquashfs", "-s", "-o", offset, image, check=False).returncode == 0:
            # Nonroot unsquashfs masks ordinary archived file modes with its
            # inherited umask. Preserve the actual modes for strict auditing;
            # only this child changes umask, leaving the parent's private 077.
            run("unsquashfs", "-no-progress", "-d", destination, "-o", offset, image, umask=0)
            return
        offset = data.find(b"hsqs", offset + 1)
    raise PackagingError(f"AppImage has no valid SquashFS payload: {image}")


def metadata_from_environment():
    fields = {
        "channel": "MAPLE_PACKAGE_CHANNEL",
        "app_name": "MAPLE_PACKAGE_APP_NAME",
        "bundle_id": "MAPLE_PACKAGE_BUNDLE_ID",
        "version": "MAPLE_PACKAGE_VERSION",
        "build_number": "MAPLE_PACKAGE_BUILD_NUMBER",
    }
    metadata = {}
    for field, variable in fields.items():
        value = os.environ.get(variable, "")
        if not value:
            raise PackagingError(f"Missing or invalid public package metadata: {variable}")
        metadata[field] = value
    return validate_metadata(metadata)


def validate_metadata(metadata):
    for field in ("channel", "app_name", "bundle_id", "version", "build_number"):
        value = metadata.get(field)
        if not isinstance(value, str) or not value or any(char in value for char in "\n\r\x00"):
            raise PackagingError(f"Missing or invalid public package metadata: {field}")
    if metadata["channel"] not in ("dev", "prod"):
        raise PackagingError("Release channel must be dev or prod")
    if not re.fullmatch(r"[A-Za-z0-9_.-]+", metadata["bundle_id"]):
        raise PackagingError("Bundle ID is invalid for desktop metadata")
    return metadata


def main():
    if len(sys.argv) == 3 and sys.argv[1] == "audit":
        appdir = Path(sys.argv[2]).absolute()
        result = audit(appdir)
        recorded = json.loads((appdir / "usr/share/maple-agent/runtime-audit.json").read_text())
        if result != recorded:
            raise PackagingError("Packaged runtime audit does not match extracted payload")
        print(f"Verified Agent AppDir: {appdir}")
        return
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--stage-only", action="store_true", help="Write an audited AppDir, not a release artifact")
    parser.add_argument("binary", type=Path)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    binary, output = args.binary.absolute(), args.output.absolute()
    if not binary.is_file() or not os.access(binary, os.X_OK):
        raise PackagingError(f"Missing executable release binary: {binary}")
    if output.exists():
        raise PackagingError(f"Refusing to replace existing output: {output}")
    require_x86_64_elf(binary)
    metadata = metadata_from_environment()
    epoch = int(os.environ["SOURCE_DATE_EPOCH"])
    if epoch < 0:
        raise PackagingError("SOURCE_DATE_EPOCH must be nonnegative")
    closure_info = Path(os.environ["MAPLE_AGENT_LINUX_CLOSURE_INFO"])
    glibc = Path(os.environ["MAPLE_AGENT_LINUX_GLIBC"])
    closure = Closure(closure_info / "store-paths")
    component = Path(__file__).resolve().parent.parent
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="maple-agent-appimage-", dir=output.parent) as temporary:
        temporary = Path(temporary)
        appdir = temporary / "MapleAgent.AppDir"
        appdir.mkdir()
        stage_closure(appdir, binary, closure, glibc)
        desktop, icon = stage_metadata(appdir, component, metadata)
        if not args.stage_only:
            tools = Path(os.environ["MAPLE_AGENT_APPIMAGE_TOOLS"])
            linuxdeploy = temporary / "linuxdeploy.AppDir"
            extract_appimage(tools / "linuxdeploy-x86_64.AppImage", linuxdeploy)
            tool_environment = os.environ.copy()
            tool_environment["APPDIR"] = str(linuxdeploy)
            tool_environment["LINUXDEPLOY_PLUGIN_MODE"] = "1"
            for name in ("APPIMAGE", "ARGV0", "APPIMAGE_EXTRACT_AND_RUN"):
                tool_environment.pop(name, None)
            # linuxdeploy validates/deploys the desktop assets in an isolated
            # metadata AppDir. Its generic ELF scan excludes glibc and can use
            # the build host's ldd; Agent's complete loader/closure must instead
            # remain exactly the payload staged and audited above.
            metadata_appdir = temporary / "metadata.AppDir"
            run(linuxdeploy / "AppRun", "--appdir", metadata_appdir, "--desktop-file", desktop, "--icon-file", icon, env=tool_environment)
            deployed_desktop = metadata_appdir / "usr/share/applications" / desktop.name
            if not deployed_desktop.is_file() or deployed_desktop.read_bytes() != desktop.read_bytes():
                raise PackagingError("linuxdeploy changed or failed to deploy Agent desktop metadata")
            deployed_icons = list((metadata_appdir / "usr/share/icons").rglob(icon.name))
            if len(deployed_icons) != 1 or digest(deployed_icons[0]) != digest(icon):
                raise PackagingError("linuxdeploy changed or failed to deploy Agent package icon")
        normalize_public_permissions(appdir)
        result = audit(appdir, metadata)
        (appdir / "usr/share/maple-agent/runtime-audit.json").write_text(json.dumps(result, indent=2) + "\n")
        (appdir / "usr/share/maple-agent/runtime-audit.json").chmod(0o644)
        for path in appdir.rglob("*"):
            os.utime(path, (epoch, epoch), follow_symlinks=False)
        if args.stage_only:
            shutil.move(appdir, output)
        else:
            squashfs = temporary / "payload.squashfs"
            # SquashFS rejects inherited SOURCE_DATE_EPOCH with explicit time
            # options. Keep both deterministic flags and scope removal to this
            # child process so the package environment remains unchanged.
            squashfs_environment = os.environ.copy()
            squashfs_environment.pop("SOURCE_DATE_EPOCH", None)
            run("mksquashfs", appdir, squashfs, "-noappend", "-no-progress", "-all-root", "-all-time", epoch, "-mkfs-time", epoch, "-comp", "gzip", "-processors", "1", env=squashfs_environment)
            packaged = temporary / "maple-agent.AppImage"
            with packaged.open("wb") as stream:
                for source in (tools / "runtime-x86_64", squashfs):
                    with source.open("rb") as part:
                        shutil.copyfileobj(part, stream)
            packaged.chmod(0o755)
            extracted = temporary / "final.AppDir"
            extract_appimage(packaged, extracted)
            if audit(extracted, metadata) != result:
                raise PackagingError("Final AppImage runtime payload differs from audited AppDir")
            shutil.move(packaged, output)
        audit_sidecar = output.with_name(output.name + ".runtime-audit.json")
        audit_sidecar.write_text(json.dumps(result, indent=2) + "\n")
        audit_sidecar.chmod(0o644)
        print(output)


if __name__ == "__main__":
    try:
        main()
    except (PackagingError, OSError, KeyError, ValueError) as error:
        print(f"Maple Agent AppImage packaging failed: {error}", file=sys.stderr)
        sys.exit(1)
