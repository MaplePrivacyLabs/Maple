#!/usr/bin/env python3
"""Build Agent's x86_64 AppImage from the owning pinned Nix shell.

Like other desktop applications, the bundle uses the host's glibc, display
libraries and GPU drivers. The release binary targets an old glibc floor and
links only libraries every Linux desktop provides, so host drivers always load
into a process with the libc they were built for. A separate clean-machine
smoke must gate publication; an ELF audit does not prove GUI/audio/CUA
integration.
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
# Oldest host glibc the release binary may require (build-release.sh passes
# the same floor to cargo-zigbuild).
GLIBC_FLOOR = (2, 31)
# The only DT_NEEDED libraries the payload may link. glibc's libraries come
# from the host loader; the rest ship with every X11/Wayland desktop.
HOST_LIBRARIES = frozenset({
    LOADER,
    "libc.so.6",
    "libm.so.6",
    "libdl.so.2",
    "libpthread.so.0",
    "librt.so.1",
    "libgcc_s.so.1",
    "libasound.so.2",
    "libX11.so.6",
    "libXi.so.6",
    "libxcb.so.1",
    "libxkbcommon.so.0",
    "libxkbcommon-x11.so.0",
})
# Opened at runtime from the host, never bundled: GPUI's Vulkan/OpenGL and
# Wayland backends, fontconfig, and the embedded CUA X11 input path.
DLOPEN_HOST_LIBRARIES = (
    "libvulkan.so.1",
    "libEGL.so.1",
    "libwayland-client.so.0",
    "libwayland-cursor.so.0",
    "libwayland-egl.so.1",
    "libX11-xcb.so.1",
    "libXtst.so.6",
    "libfontconfig.so.1",
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


def glibc_version(name):
    match = re.fullmatch(r"GLIBC_(\d+(?:\.\d+)+)", name)
    return tuple(int(part) for part in match[1].split(".")) if match else None


def stage_payload(appdir, binary):
    destination = appdir / "usr/bin/maple-agent"
    destination.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(binary, destination)
    destination.chmod(0o755)
    compatibility = appdir / "usr/lib" / LOADER
    compatibility.parent.mkdir(parents=True, exist_ok=True)
    compatibility.write_text(COMPATIBILITY_LOADER)
    compatibility.chmod(0o755)


APPRUN = r'''#!/bin/sh
set -eu
appdir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
if [ "${APPDIR:-}" != "$appdir" ]; then
    unset APPIMAGE APPIMAGE_EXTRACT_AND_RUN
fi
export APPDIR="$appdir"
# The payload runs on the host's glibc and loads the host's display libraries
# and GPU drivers, like any other desktop application.
if [ ! -e /lib64/ld-linux-x86-64.so.2 ]; then
    echo "Maple needs an x86_64 Linux system with glibc (/lib64/ld-linux-x86-64.so.2). On NixOS, use the Maple flake package or nix-ld." >&2
    exit 127
fi
exec "$appdir/usr/bin/maple-agent" "$@"
'''

# Bundle format v1 requires this executable path. Releases that bundled glibc
# check for it before installing an update, and may start a bounded child
# through it with loader options until they relaunch the new AppRun.
COMPATIBILITY_LOADER = r'''#!/bin/sh
# Compatibility entry point for Maple releases that bundled a glibc loader.
# This bundle uses the host loader, so run the payload directly.
set -eu
while [ $# -gt 0 ]; do
    case "$1" in
        --inhibit-cache) shift ;;
        --library-path) shift 2 ;;
        *) break ;;
    esac
done
appdir=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd -P)
if [ $# -eq 0 ] || [ "$1" != "$appdir/usr/bin/maple-agent" ]; then
    echo "This Maple bundle runs only its own usr/bin/maple-agent." >&2
    exit 127
fi
shift
exec "$appdir/usr/bin/maple-agent" "$@"
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
    (data / "package-metadata.json").write_text(json.dumps(metadata, indent=2) + "\n")
    (appdir / "AppRun").write_text(APPRUN)
    (appdir / "AppRun").chmod(0o755)
    return desktop, staged_icon


def launchers(appdir):
    return (appdir / "AppRun", appdir / "install.sh", appdir / "usr/lib" / LOADER)


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
            path.chmod(0o755 if path in launchers(appdir) or elf(path) else 0o644)


def audit(appdir, metadata=None):
    if metadata is None:
        metadata = validate_metadata(json.loads((appdir / "usr/share/maple-agent/package-metadata.json").read_text()))
    payload = appdir / "usr/bin/maple-agent"
    for path in (appdir, *sorted(appdir.rglob("*"))):
        if path.is_symlink() and (not path.exists() or not inside(path, appdir)):
            raise PackagingError(f"AppDir symlink escapes package or is dangling: {path}")
        if path.is_dir() and not path.is_symlink() and path.stat().st_mode & 0o777 != 0o755:
            raise PackagingError(f"Restrictive or unsafe public package directory permissions: {path}")
        if path.is_file() and not path.is_symlink():
            expected_mode = 0o755 if path in launchers(appdir) or elf(path) else 0o644
            if path.stat().st_mode & 0o7777 != expected_mode:
                raise PackagingError(f"Restrictive or unsafe public package file permissions: {path}")
            if elf(path) and path != payload:
                raise PackagingError(f"Bundled ELF besides the payload; host libraries are never bundled: {path}")
    if not payload.is_file() or payload.is_symlink() or not elf(payload):
        raise PackagingError("Missing Agent payload executable")
    require_x86_64_elf(payload)
    if interpreter(payload) != INTERPRETER:
        raise PackagingError(f"Payload must use the host loader {INTERPRETER}")
    if run("patchelf", "--print-rpath", payload).stdout.strip():
        raise PackagingError("Payload must not carry a runtime library path")
    libraries = needed(payload)
    unexpected = sorted(set(libraries) - HOST_LIBRARIES)
    if unexpected:
        raise PackagingError(f"Payload links libraries a desktop host may not provide: {unexpected}")
    if "libc.so.6" not in libraries:
        raise PackagingError("Release binary has no libc dependency; expected native GPUI build")
    required = set()
    for line in run("readelf", "--version-info", payload).stdout.splitlines():
        match = re.search(r"\bName:\s+(GLIBC_\S+)", line)
        if match:
            required.add(match[1])
    too_new = sorted(name for name in required if (glibc_version(name) or (99,)) > GLIBC_FLOOR)
    if too_new:
        floor = ".".join(map(str, GLIBC_FLOOR))
        raise PackagingError(f"Payload requires glibc newer than the {floor} floor: {too_new}")
    compatibility = appdir / "usr/lib" / LOADER
    if not compatibility.is_file() or compatibility.is_symlink() or compatibility.read_text() != COMPATIBILITY_LOADER:
        raise PackagingError("Missing bundle format v1 compatibility launcher")
    if (appdir / "AppRun").read_text() not in (APPRUN, *apprun_variants()):
        raise PackagingError("AppRun differs from the reviewed launcher")
    for path in (appdir / "AppRun", *appdir.glob("*.desktop")):
        if b"/nix/store" in path.read_bytes():
            raise PackagingError(f"Build-store path in launch metadata: {path}")
    return {
        "schema": 2,
        "channel": metadata["channel"],
        "architecture": "x86_64",
        "bundled_glibc": False,
        "interpreter": INTERPRETER,
        "glibc_floor": ".".join(map(str, GLIBC_FLOOR)),
        "payload_sha256": digest(payload),
        "required_glibc_versions": sorted(required, key=lambda name: glibc_version(name) or (99,)),
        "host_libraries": sorted(libraries),
        "dlopen_host_libraries": list(DLOPEN_HOST_LIBRARIES),
        "host_resources": ["GPU drivers", "fontconfig", "ALSA routing/plugins"],
    }


def apprun_variants():
    # The tar bundle never re-enters an inherited outer AppImage.
    legacy = 'if [ "${APPDIR:-}" != "$appdir" ]; then\n    unset APPIMAGE APPIMAGE_EXTRACT_AND_RUN\nfi\n'
    return (APPRUN.replace(legacy, "unset APPIMAGE APPIMAGE_EXTRACT_AND_RUN\n", 1),)


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
    component = Path(__file__).resolve().parent.parent
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="maple-agent-appimage-", dir=output.parent) as temporary:
        temporary = Path(temporary)
        appdir = temporary / "MapleAgent.AppDir"
        appdir.mkdir()
        stage_payload(appdir, binary)
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
            # metadata AppDir. Its generic ELF scan can use the build host's
            # ldd; Agent's payload must instead remain exactly the binary
            # staged and audited above.
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
