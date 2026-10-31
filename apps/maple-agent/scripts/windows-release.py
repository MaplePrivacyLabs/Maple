#!/usr/bin/env python3
"""Package and verify native Agent Windows installers, separately from builds.

The package path treats the prebuilt executable as data, even before signing.
Only an explicit --smoke on a credential-free Windows verifier runs the app.
"""

import argparse
from contextlib import contextmanager
import gzip
import hashlib
import importlib.util
import json
import ntpath
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile
import threading
import time
import urllib.request
import uuid
import zipfile


SCRIPTS = Path(__file__).resolve().parent
COMPONENT = SCRIPTS.parent
PLATFORM = "windows-x86_64"
WINDOWS_ARCHITECTURES = ("x86_64", "aarch64")
NSIS_VERSION = "3.12"
NSIS_URL = "https://downloads.sourceforge.net/project/nsis/NSIS%203/3.12/nsis-3.12.zip"
NSIS_SHA256 = "56581f90db321581c5381193d796fffcf2d24b2f8fed2160a6c6a3baa67f2c4f"
# Windows 10 version 1703+ OS components only. No compiler-installation, VC redistributable,
# crypto-provider or application DLL may be supplied by the build runner's PATH.
SYSTEM_IMPORTS = frozenset("""
    advapi32.dll avrt.dll bcrypt.dll bcryptprimitives.dll cabinet.dll cfgmgr32.dll
    combase.dll comctl32.dll comdlg32.dll credui.dll crypt32.dll cryptbase.dll cryptnet.dll
    cryptsp.dll d2d1.dll d3d11.dll d3d12.dll d3dcompiler_47.dll dbghelp.dll
    dcomp.dll dhcpcsvc.dll dnsapi.dll dwmapi.dll dwrite.dll dxgi.dll
    gdi32.dll glu32.dll hid.dll icuuc.dll imm32.dll iphlpapi.dll kernel32.dll kernelbase.dll
    mf.dll mfplat.dll mfreadwrite.dll mmdevapi.dll msasn1.dll msimg32.dll
    msctf.dll msvcrt.dll mswsock.dll ncrypt.dll netapi32.dll normaliz.dll ntdll.dll ole32.dll oleacc.dll
    oleaut32.dll opengl32.dll powrprof.dll propsys.dll psapi.dll rasapi32.dll
    rpcrt4.dll secur32.dll setupapi.dll shcore.dll shell32.dll shlwapi.dll
    synchronization.dll ucrtbase.dll uiautomationcore.dll urlmon.dll user32.dll userenv.dll usp10.dll
    uxtheme.dll version.dll wer.dll wevtapi.dll windowscodecs.dll winhttp.dll wininet.dll
    winmm.dll winspool.drv wintrust.dll winusb.dll wldap32.dll ws2_32.dll wtsapi32.dll
""".split())
PAYLOAD_FILES = {"maple-agent.exe", "LICENSE.txt", "build-info.json"}
BUNDLE_ROOT = "maple-agent-windows"
BUNDLE_FORMAT = "maple-agent-windows-bundle-v2"
BUNDLE_FILES = PAYLOAD_FILES | {"bundle-info.json"}
BUNDLE_DIRECTORIES = {""}
MAX_BUNDLE_BYTES = 768 * 1024 * 1024
INSTALLER_CHECKER = "$PLUGINSDIR/maple-installer-check.exe"
DIAGNOSTIC_LIMIT = 8192
NSIS_FAILURE_PHASES = {
    10: "architecture-probe", 11: "unsupported-architecture", 12: "version-probe",
    13: "unsupported-os", 20: "checker-extraction", 21: "preflight-launch",
    22: "preflight-child", 23: "prepare-launch", 24: "prepare-child",
    30: "payload-copy", 31: "registration", 40: "activation-launch",
    41: "activation-child", 50: "uninstall-registration", 51: "uninstall-launch",
    52: "uninstall-child", 53: "uninstall-files",
}
ACTIVATION_FAILURE_MARKER = 0x4D000000
ACTIVATION_FAILURE_STAGES = {
    1: "activation-input", 2: "root-storage", 3: "registration",
    4: "bootstrap-identity", 5: "install-lock", 6: "intent-identity",
    7: "generation-identity", 8: "generation-verification",
    9: "generation-guards", 10: "pointer-commit",
}


def platform_name(arch):
    if arch not in WINDOWS_ARCHITECTURES:
        raise ValueError("unsupported Windows architecture")
    return "windows-" + arch


def require_native_host(arch):
    """Do not count emulated installation as a native architecture smoke."""
    import ctypes

    platform_name(arch)
    process_machine, native_machine = ctypes.c_ushort(), ctypes.c_ushort()
    kernel = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel.GetCurrentProcess.argtypes, kernel.GetCurrentProcess.restype = (), ctypes.c_void_p
    kernel.IsWow64Process2.argtypes = (ctypes.c_void_p, ctypes.POINTER(ctypes.c_ushort), ctypes.POINTER(ctypes.c_ushort))
    kernel.IsWow64Process2.restype = ctypes.c_int
    # Unlike GetNativeSystemInfo, IsWow64Process2 reports ARM64 even when this
    # Python/tool process itself uses Windows' x64 emulation.
    if not kernel.IsWow64Process2(kernel.GetCurrentProcess(), ctypes.byref(process_machine), ctypes.byref(native_machine)):
        raise ctypes.WinError(ctypes.get_last_error())
    if native_machine.value != pe.MACHINES[arch]:
        raise ValueError("Windows smoke requires the selected native machine architecture")


def decode_activation_failure(code):
    # This fixed status is emitted by the actual native activation child and
    # passed through NSIS. No helper replay or arbitrary child output is read.
    if type(code) is not int or not 0 <= code <= 0xFFFFFFFF or code & 0xFF000000 != ACTIVATION_FAILURE_MARKER:
        return None
    stage = (code >> 16) & 0xFF
    if stage not in ACTIVATION_FAILURE_STAGES:
        return None
    return {"native_stage": ACTIVATION_FAILURE_STAGES[stage],
            "native_win32_error": (code & 0xFFFF) or None}


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


def regular(path):
    if path.is_symlink() or not path.is_file():
        raise ValueError("Windows payload requires regular files")
    # Windows junction/reparse points must not redirect a trusted package read.
    for parent in (path, *path.parents):
        if getattr(parent.lstat(), "st_file_attributes", 0) & 0x400:
            raise ValueError("Windows payload cannot traverse a reparse point")


def bundle_payload_inventory(directory):
    """Reject extra directories and link/reparse entries before archiving."""
    files, directories = set(), {""}
    if directory.is_symlink() or not directory.is_dir():
        raise ValueError("Windows updater payload requires an ordinary directory")
    for path in (directory, *directory.rglob("*")):
        if path.is_symlink() or getattr(path.lstat(), "st_file_attributes", 0) & 0x400:
            raise ValueError("Windows updater payload cannot contain links or reparse points")
        name = path.relative_to(directory).as_posix()
        if path == directory:
            continue
        if path.is_dir():
            directories.add(name)
        elif path.is_file():
            regular(path)
            files.add(name)
        else:
            raise ValueError("Windows updater payload contains an unsupported entry")
    if files != BUNDLE_FILES or directories != BUNDLE_DIRECTORIES:
        raise ValueError("Updater bundle requires the exact standalone Windows installer payload")
    return files


def validate_imports(binary):
    imports = pe.inspect_pe(binary)["imports"]
    unknown = [name for name in imports if name not in SYSTEM_IMPORTS and not re.fullmatch(r"(?:api|ext)-ms-win-[a-z0-9-]+\.dll", name)]
    if unknown:
        raise ValueError("Windows release has unbundled non-OS DLL imports: " + ", ".join(unknown))


def prepare(profile, arch="x86_64"):
    binary = COMPONENT / "target/release/maple-agent.exe"
    regular(binary)
    info = pe.validated_build_info(profile, binary, source_sha(), arch)
    validate_imports(binary)
    print("Verified exact Windows binary profile, source, DLL closure")
    return binary, info


def signature(file, unsigned, *, sign=False):
    command = ["pwsh", "-NoLogo", "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", SCRIPTS / "windows-sign-file.ps1", "-File", file]
    if not sign:
        command.append("-VerifyOnly")
    if unsigned:
        command.append("-Unsigned")
    run(command, timeout=300)


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
    signer = SCRIPTS / "windows-sign-file.ps1"
    # !uninstfinalize is a compiler command string. Reject cmd.exe expansion
    # syntax in the trusted script's filesystem path rather than quoting it
    # as though a JSON string or argv list controlled cmd.exe parsing.
    if any(char in str(signer) for char in '%&|<>^"\r\n'):
        raise ValueError("Unsafe path for the native uninstaller signing command")
    values["SIGNER"] = signer
    if unsigned:
        values["UNSIGNED_UNINSTALLER"] = "1"
    path.write_text("".join(f'!define {key} "{nsis_literal(value)}"\n' for key, value in values.items()), encoding="utf-8", newline="\n")


def unpack_nsis(archive, destination):
    if hashlib.sha256(archive.read_bytes()).hexdigest() != NSIS_SHA256:
        raise ValueError("NSIS archive does not match the reviewed pin")
    with zipfile.ZipFile(archive) as contents:
        members = contents.infolist()
        if len(members) > 4096 or sum(item.file_size for item in members) > 64 * 1024 * 1024:
            raise ValueError("NSIS archive exceeds extraction bounds")
        for item in members:
            name = PurePosixPath(item.filename)
            if name.is_absolute() or ".." in name.parts or not name.parts or name.parts[0] != f"nsis-{NSIS_VERSION}" or "\\" in item.filename or ":" in item.filename:
                raise ValueError("Unsafe NSIS archive path")
            if stat.S_ISLNK(item.external_attr >> 16):
                raise ValueError("NSIS archive cannot contain symlinks")
        contents.extractall(destination)
    compiler = destination / f"nsis-{NSIS_VERSION}/makensis.exe"
    regular(compiler)
    return compiler


def install_nsis(scratch):
    archive = scratch / "nsis.zip"
    with urllib.request.urlopen(NSIS_URL, timeout=90) as response:
        data = response.read(8 * 1024 * 1024 + 1)
    if len(data) > 8 * 1024 * 1024:
        raise ValueError("NSIS download exceeds the reviewed archive limit")
    archive.write_bytes(data)
    return unpack_nsis(archive, scratch / "nsis")


def seven_zip():
    for candidate in (Path(os.environ.get("ProgramFiles", "C:/Program Files")) / "7-Zip/7z.exe", shutil.which("7z"), shutil.which("7z.exe")):
        if candidate and Path(candidate).is_file():
            return str(candidate)
    raise ValueError("The native Windows verifier requires 7-Zip")


def extract_member(extractor, installer, name, destination, limit):
    destination.parent.mkdir(parents=True, exist_ok=True)
    process = subprocess.Popen([extractor, "e", "-so", str(installer), name], stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    timer = threading.Timer(120, process.kill)
    timer.start()
    try:
        total = 0
        with destination.open("xb") as output:
            while block := process.stdout.read(64 * 1024):
                total += len(block)
                if total > limit:
                    raise ValueError("Windows installer member exceeds extraction bounds")
                output.write(block)
        if process.wait(timeout=5) != 0:
            raise ValueError("Could not extract the exact Windows installer member")
        return total
    finally:
        timer.cancel()
        if process.poll() is None:
            process.kill()
        process.wait(timeout=5)
        process.stdout.close()


def extract_installer(installer, destination, unsigned, info, build):
    extractor = seven_zip()
    listing = run([extractor, "l", "-slt", "-ba", installer], capture_output=True, text=True, encoding="utf-8").stdout
    if len(listing) > 256 * 1024:
        raise ValueError("Windows installer inventory is too large")
    files = {}
    for record in re.split(r"\r?\n\r?\n", listing.strip()):
        fields = dict(line.split(" = ", 1) for line in record.splitlines() if " = " in line)
        if "Path" not in fields or fields.get("Folder") == "+":
            continue
        name = fields["Path"].replace("\\", "/")
        relative = PurePosixPath(name)
        if relative.is_absolute() or ".." in relative.parts or ":" in name or name in files:
            raise ValueError("Unsafe or duplicate Windows installer payload path")
        files[name] = fields
    generation = info["version"] + "-" + build
    payload_prefix = "versions/" + generation + "/"
    expected = {payload_prefix + name for name in BUNDLE_FILES} | {"maple-agent.exe", "uninstall.exe", INSTALLER_CHECKER}
    # !uninstfinalize emits the actual standalone PE for signed and preview
    # installers. The root uninstaller is not part of versioned update payloads.
    if set(files) != expected:
        raise ValueError("Windows installer has an unexpected payload inventory")
    uninstaller = "uninstall.exe"
    # 7-Zip cannot always report a reconstructed uninstaller's size before
    # extraction. Every member is streamed through a hard bound; only that one
    # NSIS-generated member may omit its size in the preliminary inventory.
    if any(not item.get("Size", "").isdecimal() for name, item in files.items() if name != uninstaller) or (
        files[uninstaller].get("Size", "") and not files[uninstaller]["Size"].isdecimal()
    ):
        raise ValueError("Windows installer has an invalid payload size")
    declared_size = sum(int(item["Size"]) for item in files.values() if item.get("Size"))
    if declared_size > 768 * 1024 * 1024:
        raise ValueError("Windows installer exceeds payload extraction bounds")
    total = 0
    for name, item in files.items():
        declared = int(item["Size"]) if item.get("Size") else None
        limit = min(32 * 1024 * 1024 if name == uninstaller else 768 * 1024 * 1024, 768 * 1024 * 1024 - total)
        actual_size = extract_member(extractor, installer, name, destination / name, limit)
        if declared is not None and actual_size != declared:
            raise ValueError("Windows installer member differs from its declared size")
        total += actual_size
    actual = {path.relative_to(destination).as_posix() for path in destination.rglob("*") if path.is_file()}
    if actual != set(files):
        raise ValueError("Extracted Windows installer differs from its inventory")
    for name in actual:
        regular(destination / name)
    payload = destination / "versions" / generation
    generation_executable = (payload / "maple-agent.exe").read_bytes()
    for name in ("maple-agent.exe", INSTALLER_CHECKER):
        if (destination / name).read_bytes() != generation_executable:
            raise ValueError("Windows bootstrap or preflight checker differs from the signed generation executable")
    descriptor = json.loads((payload / "bundle-info.json").read_bytes(), object_pairs_hook=pe.unique_object)
    if descriptor != bundle_descriptor(info, build):
        raise ValueError("Windows installer generation identity differs")
    return destination / uninstaller


def verify_payload(profile, directory, expected_info, unsigned, *, verify_signature=True, arch="x86_64"):
    binary = directory / "maple-agent.exe"
    info = pe.validated_build_info(profile, binary, source_sha(), arch)
    if info != expected_info or json.loads((directory / "build-info.json").read_text(encoding="utf-8")) != info:
        raise ValueError("Installer payload identity differs from its verified build")
    validate_imports(binary)
    pe.verify_icon(binary, COMPONENT / "app/packaging" / ("maple-agent-dev.ico" if profile == "dev" else "maple-agent.ico"))
    if (directory / "LICENSE.txt").read_bytes() != (COMPONENT / "LICENSE").read_bytes():
        raise ValueError("Installer license differs from the reviewed source")
    if verify_signature:
        signature(binary, unsigned)


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


def write_bundle(payload, archive_path, info, build):
    """Package the exact extracted NSIS files, never execute a downloaded PE."""
    bundle_payload_inventory(payload)
    descriptor = json.dumps(bundle_descriptor(info, build), sort_keys=True, separators=(",", ":")) + "\n"
    if (payload / "bundle-info.json").read_text(encoding="utf-8") != descriptor:
        raise ValueError("Updater bundle descriptor differs from the verified installer")
    total = sum((payload / name).stat().st_size for name in BUNDLE_FILES)
    if total > MAX_BUNDLE_BYTES:
        raise ValueError("Windows updater payload exceeds extraction bounds")
    with archive_path.open("xb") as target:
        with gzip.GzipFile(fileobj=target, mode="wb", filename="", mtime=0) as compressed:
            with tarfile.open(fileobj=compressed, mode="w", format=tarfile.USTAR_FORMAT) as archive:
                for name in sorted(BUNDLE_DIRECTORIES):
                    entry = tarfile.TarInfo(BUNDLE_ROOT + ("/" + name if name else ""))
                    entry.type, entry.mode = tarfile.DIRTYPE, 0o755
                    archive.addfile(entry)
                for name in sorted(BUNDLE_FILES):
                    file = payload / name
                    entry = tarfile.TarInfo(BUNDLE_ROOT + "/" + name)
                    entry.size, entry.mode = file.stat().st_size, 0o644
                    with file.open("rb") as data:
                        archive.addfile(entry, data)
    inspect_bundle(archive_path, info, build)


def preflight_bundle_headers(archive_path):
    """Reject extensions before tarfile can consume attacker-controlled metadata."""
    regular(archive_path)
    if archive_path.stat().st_size > MAX_BUNDLE_BYTES:
        raise ValueError("Windows updater archive exceeds bounds")
    total, count = 0, 0
    with gzip.open(archive_path, "rb") as stream:
        while True:
            header = stream.read(512)
            if len(header) != 512:
                raise ValueError("Windows updater archive is truncated")
            if header == bytes(512):
                if stream.read(512) != bytes(512):
                    raise ValueError("Windows updater archive has an invalid end marker")
                trailing = stream.read(10241)
                if len(trailing) > 10240 or any(trailing):
                    raise ValueError("Windows updater archive has trailing content")
                return
            count += 1
            entry = tarfile.TarInfo.frombuf(header, "utf-8", "strict")
            if count > 128 or entry.type not in (tarfile.REGTYPE, tarfile.AREGTYPE, tarfile.DIRTYPE):
                raise ValueError("Windows updater archive contains unsupported entries")
            if not 0 <= entry.size <= MAX_BUNDLE_BYTES or (entry.isdir() and entry.size):
                raise ValueError("Windows updater archive has an invalid member size")
            total += entry.size
            if total > MAX_BUNDLE_BYTES:
                raise ValueError("Windows updater archive exceeds expanded bounds")
            remaining = (entry.size + 511) // 512 * 512
            while remaining:
                block = stream.read(min(remaining, 1024 * 1024))
                if not block:
                    raise ValueError("Windows updater archive payload is truncated")
                remaining -= len(block)


def inspect_bundle(archive_path, info, build, extracted=None):
    """Inspect fixed public contents and optionally compare the signed installer."""
    preflight_bundle_headers(archive_path)
    expected = {BUNDLE_ROOT + "/" + name for name in BUNDLE_FILES}
    expected_dirs = {BUNDLE_ROOT + ("/" + name if name else "") for name in BUNDLE_DIRECTORIES}
    seen, digests, descriptor = set(), {}, None
    with tarfile.open(archive_path, "r:gz") as archive:
        for entry in archive:
            name = entry.name
            if name in seen or entry.linkname or entry.pax_headers or name not in expected | expected_dirs:
                raise ValueError("Windows updater archive has an unexpected or duplicate path")
            seen.add(name)
            if name in expected_dirs:
                if not entry.isdir() or entry.mode != 0o755:
                    raise ValueError("Windows updater archive has an invalid directory")
                continue
            if not entry.isfile() or entry.mode != 0o644:
                raise ValueError("Windows updater archive requires ordinary public files")
            source = archive.extractfile(entry)
            if source is None:
                raise ValueError("Windows updater archive member is unavailable")
            digest = hashlib.sha256()
            data = bytearray()
            while block := source.read(1024 * 1024):
                digest.update(block)
                if name.endswith("/bundle-info.json"):
                    data.extend(block)
                    if len(data) > 16 * 1024:
                        raise ValueError("Windows updater descriptor exceeds bounds")
            relative = name[len(BUNDLE_ROOT) + 1:]
            digests[relative] = digest.hexdigest()
            if relative == "bundle-info.json":
                descriptor = json.loads(data, object_pairs_hook=pe.unique_object)
            if extracted is not None:
                original = extracted / relative
                regular(original)
                if digest.hexdigest() != hashlib.sha256(original.read_bytes()).hexdigest():
                    raise ValueError("Windows updater bundle differs from the verified installer payload")
    if seen != expected | expected_dirs or descriptor != bundle_descriptor(info, build):
        raise ValueError("Windows updater archive identity or inventory differs")
    return digests


def package(profile, unsigned, arch="x86_64"):
    platform = platform_name(arch)
    binary, info = prepare(profile, arch)
    build = build_number()
    destination = COMPONENT / "dist" / profile
    if destination.exists():
        raise ValueError("Move the previous profile artifact directory before packaging")
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
        signature(payload / "maple-agent.exe", unsigned, sign=not unsigned)
        verify_payload(profile, payload, info, unsigned, arch=arch)
        installer = output / release_info.windows_package_name(info, platform)
        configuration = scratch / "package.nsh"
        write_nsis_config(configuration, info, payload, installer, unsigned, build, arch)
        compiler = install_nsis(scratch)
        run([compiler, "/V3", f"/DMAPLE_CONFIG={configuration}", SCRIPTS / "windows-installer.nsi"], timeout=300)
        signature(installer, unsigned, sign=not unsigned)
        extracted = scratch / "extracted"
        extracted.mkdir()
        uninstaller = extract_installer(installer, extracted, unsigned, info, build)
        if uninstaller.name != "uninstall.exe":
            raise ValueError("Windows updater requires a standalone PE uninstaller")
        generation = extracted / "versions" / (info["version"] + "-" + build)
        verify_payload(profile, generation, info, unsigned, arch=arch)
        signature(uninstaller, unsigned)
        write_bundle(generation, output / release_info.windows_archive_name(info, platform), info, build)
        shutil.copyfile(payload / "build-info.json", output / "build-info.json")
        release_info.write_manifest(info, output, platform, unsigned, build)
        # Data-only parsing of the compressed final signed bytes; never execute
        # the app, generated uninstaller or installer on the signing worker.
        verify(profile, output, unsigned, smoke=False, arch=arch)
        output.rename(destination)
    print(destination)


def smoke_environment(scratch):
    # Keep Windows process/session basics, excluding CI, Azure, provider and
    # account credentials. Use OS-only PATH to catch developer-runtime leaks.
    allowed = {"SYSTEMROOT", "WINDIR", "COMSPEC", "USERPROFILE", "APPDATA", "LOCALAPPDATA", "USERNAME", "USERDOMAIN", "PROGRAMDATA", "PROGRAMFILES", "PROGRAMFILES(X86)", "COMMONPROGRAMFILES", "COMMONPROGRAMFILES(X86)", "NUMBER_OF_PROCESSORS", "PROCESSOR_ARCHITECTURE"}
    environment = {key: value for key, value in os.environ.items() if key.upper() in allowed}
    windows = Path(os.environ["SystemRoot"])
    environment["PATH"] = os.pathsep.join(str(windows / part) for part in ("System32", "System32/WindowsPowerShell/v1.0"))
    for name in ("config", "data", "cache", "tmp"):
        (scratch / name).mkdir()
    environment.update(XDG_CONFIG_HOME=str(scratch / "config"), XDG_DATA_HOME=str(scratch / "data"), XDG_CACHE_HOME=str(scratch / "cache"), TEMP=str(scratch / "tmp"), TMP=str(scratch / "tmp"), MAPLE_DISABLE_UPDATE_CHECK="1", RUST_LOG="warn,maple_agent=debug,gpui_windows=info")
    return environment


def capture_preflight(arguments, environment, timeout=120):
    """Bound exact-child output for preflight and isolated metadata fixture tools."""
    command = [str(value) for value in arguments]
    process = subprocess.Popen(command, env=environment, stdin=subprocess.DEVNULL,
                               stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    expired = threading.Event()
    def expire():
        expired.set()
        process.kill()
    timer = threading.Timer(timeout, expire)
    timer.start()
    output = bytearray()
    try:
        while block := process.stdout.read(min(1024, DIAGNOSTIC_LIMIT + 1 - len(output))):
            output.extend(block)
            if len(output) > DIAGNOSTIC_LIMIT:
                bounded_diagnostics(output[:DIAGNOSTIC_LIMIT])
                raise ValueError("Read-only Windows preflight exceeded diagnostic bounds")
        code = process.wait(timeout=5)
        if expired.is_set():
            bounded_diagnostics(output)
            raise subprocess.TimeoutExpired(command, timeout)
        return subprocess.CompletedProcess(command, code, bytes(output))
    finally:
        timer.cancel()
        if process.poll() is None:
            process.kill()
        process.wait(timeout=5)
        process.stdout.close()


class WindowsShortcutCOM:
    """Narrow IShellLinkW/IPersistFile ABI; no automation server or Resolve."""

    def __init__(self, phase=None):
        import ctypes

        self.ctypes, self.phase = ctypes, phase or (lambda _name: None)
        self.hresult = ctypes.c_int32
        class GUID(ctypes.Structure):
            _fields_ = [("data1", ctypes.c_uint32), ("data2", ctypes.c_uint16),
                        ("data3", ctypes.c_uint16), ("data4", ctypes.c_ubyte * 8)]
        self.guid_type = GUID
        self.ole = ctypes.WinDLL("ole32", use_last_error=True)
        self.ole.CoInitializeEx.argtypes, self.ole.CoInitializeEx.restype = (ctypes.c_void_p, ctypes.c_uint32), self.hresult
        self.ole.CoUninitialize.argtypes, self.ole.CoUninitialize.restype = (), None
        self.ole.CoCreateInstance.argtypes = (ctypes.POINTER(GUID), ctypes.c_void_p, ctypes.c_uint32, ctypes.POINTER(GUID), ctypes.POINTER(ctypes.c_void_p))
        self.ole.CoCreateInstance.restype = self.hresult
        self.ole.CoTaskMemFree.argtypes, self.ole.CoTaskMemFree.restype = (ctypes.c_void_p,), None

    def guid(self, value):
        return self.guid_type.from_buffer_copy(uuid.UUID(value).bytes_le)

    def check(self, result, operation):
        if self.hresult(result).value < 0:
            raise OSError(f"Windows shortcut {operation} failed (HRESULT 0x{result & 0xFFFFFFFF:08x})")

    def __enter__(self):
        self.phase("com-init")
        self.check(self.ole.CoInitializeEx(None, 2), "STA initialization")
        return self

    def __exit__(self, *_error):
        try:
            self.phase("com-release")
        finally:
            self.ole.CoUninitialize()

    def method(self, interface, slot, result, *arguments):
        c = self.ctypes
        table = c.cast(interface, c.POINTER(c.POINTER(c.c_void_p))).contents
        return c.WINFUNCTYPE(result, c.c_void_p, *arguments)(table[slot])

    def call(self, interface, slot, *arguments, types=(), operation):
        self.check(self.method(interface, slot, self.hresult, *types)(interface, *arguments), operation)

    def release(self, interface):
        if interface.value:
            self.method(interface, 2, self.ctypes.c_uint32)(interface)
            interface.value = None

    def string(self, interface, slot, *, path=False, icon_index=None):
        c = self.ctypes
        buffer = c.create_unicode_buffer(32768)
        types, arguments = (c.c_wchar_p, c.c_int32), (buffer, len(buffer))
        if path:
            types, arguments = (*types, c.c_void_p, c.c_uint32), (*arguments, None, 4)  # SLGP_RAWPATH.
        if icon_index is not None:
            types, arguments = (*types, c.POINTER(c.c_int32)), (*arguments, c.byref(icon_index))
        self.call(interface, slot, *arguments, types=types, operation="read string")
        if len(buffer.value) >= len(buffer) - 1:
            raise ValueError("Windows shortcut field exceeds its Unicode bound")
        return buffer.value

    def metadata(self, link):
        c = self.ctypes
        hotkey, show, index = c.c_uint16(), c.c_int32(), c.c_int32()
        self.call(link, 12, c.byref(hotkey), types=(c.POINTER(c.c_uint16),), operation="read hotkey")
        self.call(link, 14, c.byref(show), types=(c.POINTER(c.c_int32),), operation="read show command")
        icon = self.string(link, 16, icon_index=index)
        return {"target": self.string(link, 3, path=True), "arguments": self.string(link, 10),
                "working_directory": self.string(link, 8), "description": self.string(link, 6),
                "hotkey": hotkey.value, "window_style": show.value, "icon_location": f"{icon},{index.value}"}

    def probe(self, path, expected_target, *, seed=False, working_directory=None, description="Maple icon migration smoke fixture"):
        c = self.ctypes
        link, persist = c.c_void_p(), c.c_void_p()
        try:
            self.phase("shell-create")
            clsid = self.guid("00021401-0000-0000-c000-000000000046")
            iid = self.guid("000214f9-0000-0000-c000-000000000046")
            self.check(self.ole.CoCreateInstance(c.byref(clsid), None, 1, c.byref(iid), c.byref(link)), "create ShellLinkW")
            if not link.value:
                raise ValueError("Windows shortcut returned a null ShellLinkW")
            self.phase("persist-query")
            iid = self.guid("0000010b-0000-0000-c000-000000000046")
            self.call(link, 0, c.byref(iid), c.byref(persist), types=(c.POINTER(self.guid_type), c.POINTER(c.c_void_p)), operation="query IPersistFile")
            if not persist.value:
                raise ValueError("Windows shortcut returned a null IPersistFile")
            self.phase("link-load")
            self.call(persist, 5, str(path), 0, types=(c.c_wchar_p, c.c_uint32), operation="load")
            self.phase("link-read")
            metadata = self.metadata(link)
            mismatch = ("Shortcut is not owned by this smoke install "
                        f"(target={repr(metadata['target'])[:512]}, expected={repr(str(expected_target))[:512]}, "
                        f"arguments_empty={metadata['arguments'] == ''})")
            try:
                owned = metadata["arguments"] == "" and windows_shortcut_target_equal(metadata["target"], expected_target)
            except (OSError, ValueError) as error:
                raise ValueError(mismatch) from error
            if not owned:
                raise ValueError(mismatch)
            if seed:
                self.phase("link-seed")
                self.call(link, 17, str(expected_target), 0, types=(c.c_wchar_p, c.c_int32), operation="set fixture icon")
                for slot, value in ((9, working_directory), (7, description)):
                    if value is not None:
                        self.call(link, slot, str(value), types=(c.c_wchar_p,), operation="set fixture property")
                regular(path)
                self.phase("link-save")
                self.call(persist, 6, str(path), 1, types=(c.c_wchar_p, c.c_int32), operation="save fixture")
                metadata = self.metadata(link)
            return metadata
        finally:
            try:
                self.phase("link-release")
            finally:
                try:
                    self.release(persist)
                finally:
                    self.release(link)


def probe_windows_shortcut(path, expected_target, *, seed=False, working_directory=None,
                           description="Maple icon migration smoke fixture", phase=None):
    """Read/seed one explicit owned link; also used by the fast native fixture."""
    path, expected_target = Path(path), Path(expected_target)
    regular(path)
    for value in (path, expected_target, working_directory, description):
        if value is not None and ("\0" in str(value) or len(str(value)) >= 32767):
            raise ValueError("Windows shortcut input exceeds its Unicode bound")
    with WindowsShortcutCOM(phase) as native:
        return native.probe(path, expected_target, seed=seed, working_directory=working_directory, description=description)


def windows_programs_directory():
    with WindowsShortcutCOM() as native:
        c = native.ctypes
        shell = c.WinDLL("shell32", use_last_error=True)
        shell.SHGetKnownFolderPath.argtypes = (c.POINTER(native.guid_type), c.c_uint32, c.c_void_p, c.POINTER(c.c_void_p))
        shell.SHGetKnownFolderPath.restype = native.hresult
        folder = native.guid("a77f5d77-2e2b-44c3-a6a2-aba601054a51")  # FOLDERID_Programs.
        pointer = c.c_void_p()
        try:
            native.check(shell.SHGetKnownFolderPath(c.byref(folder), 0, None, c.byref(pointer)), "locate Programs")
            if not pointer.value:
                raise ValueError("Windows Programs folder returned a null path")
            characters = c.cast(pointer, c.POINTER(c.c_wchar))
            value = []
            for index in range(32768):
                if characters[index] == "\0":
                    return Path("".join(value))
                value.append(characters[index])
            raise ValueError("Windows Programs folder exceeds its Unicode bound")
        finally:
            native.ole.CoTaskMemFree(pointer)


SHORTCUT_PROBE_CHILD = r'''
import importlib.util, json, sys
from pathlib import Path
spec = importlib.util.spec_from_file_location("windows_release_probe", sys.argv[1])
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
display_name, installed, working, mode, checkpoint = sys.argv[2:]
installed, working, checkpoint = Path(installed), Path(working), Path(checkpoint)
def phase(name):
    checkpoint.write_text(name, encoding="ascii")
phase("known-folder")
directory = module.windows_programs_directory() / display_name
if mode not in ("seed", "read"):
    raise ValueError("Invalid shortcut probe operation")
metadata = {}
for name, filename, target in (("main", display_name + ".lnk", "maple-agent.exe"), ("uninstall", "Uninstall.lnk", "uninstall.exe")):
    metadata[name] = module.probe_windows_shortcut(directory / filename, installed / target,
        seed=mode == "seed", working_directory=working, phase=phase)
print(json.dumps(metadata, sort_keys=True, separators=(",", ":")), flush=True)
'''


class WindowsIntegrationProbe:
    """Seed only this smoke install's legacy links; inspect real Shell metadata."""

    def __init__(self, info, installed, scratch, environment, winreg):
        self.info, self.installed, self.environment, self.winreg = info, installed, environment, winreg
        canonical = COMPONENT / "app/packaging" / ("maple-agent-dev.ico" if info["profile"] == "dev" else "maple-agent.ico")
        regular(canonical)
        self.artwork = canonical.read_bytes()
        self.icon = scratch / "data" / info["data_namespace"] / "windows-integration" / (hashlib.sha256(self.artwork).hexdigest() + ".ico")
        self.registry = "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\" + info["bundle_id"]
        self.scratch, self.before = scratch, None
        self.checkpoint = scratch / "shortcut-probe-phase.txt"
        self.probe_module = Path(__file__).resolve()

    def shortcuts(self, *, seed=False, timeout=20):
        # Keep the existing exact-child deadline/output bounds around native
        # COM too. The child uses this same direct API as the fast Rust fixture,
        # without PowerShell, Windows Script Host, Resolve or shell automation.
        self.checkpoint.write_text("python-start", encoding="ascii")
        arguments = [sys.executable, "-I", "-c", SHORTCUT_PROBE_CHILD, self.probe_module,
                     self.info["display_name"], self.installed, self.scratch,
                     "seed" if seed else "read", self.checkpoint]
        try:
            result = capture_preflight(arguments, self.environment, timeout=min(20, timeout))
        except subprocess.SubprocessError:
            self.report_phase()
            raise
        if result.returncode:
            self.report_phase()
            bounded_diagnostics(result.stdout)
            raise subprocess.CalledProcessError(result.returncode, result.args)
        metadata = json.loads(result.stdout.decode("utf-8-sig"), object_pairs_hook=pe.unique_object)
        fields = {"target", "arguments", "working_directory", "description", "hotkey", "window_style", "icon_location"}
        if type(metadata) is not dict or set(metadata) != {"main", "uninstall"} or any(type(value) is not dict or set(value) != fields for value in metadata.values()):
            raise ValueError("Windows shortcut probe returned an invalid inventory")
        return metadata

    def report_phase(self):
        try:
            regular(self.checkpoint)
            with self.checkpoint.open("rb") as checkpoint:
                value = checkpoint.read(64)
            if value in {name.encode("ascii") for name in ("python-start", "known-folder", "com-init", "shell-create", "persist-query", "link-load", "link-read", "link-seed", "link-save", "link-release", "com-release")}:
                print("Windows shortcut probe stopped at: " + value.decode("ascii"), file=sys.stderr)
        except (OSError, ValueError):
            pass  # Diagnostic failure cannot replace the actual child failure.

    def registration(self, *, write=False):
        access = self.winreg.KEY_READ | self.winreg.KEY_WOW64_32KEY
        if write:
            access |= self.winreg.KEY_SET_VALUE
        return self.winreg.OpenKey(self.winreg.HKEY_CURRENT_USER, self.registry, 0, access)

    def registered_root(self, key):
        if not windows_path_equal(self.winreg.QueryValueEx(key, "InstallLocation")[0], self.installed):
            raise ValueError("Windows icon probe registration changed installation root")

    def seed(self, timeout=20):
        # Both the icon reference and non-icon properties have the same legacy
        # shape as old installers. Known marker properties prove that refresh
        # changes only the icon, rather than recreating the shortcut.
        with self.registration(write=True) as key:
            self.registered_root(key)
            self.before = self.shortcuts(seed=True, timeout=timeout)
            self.winreg.SetValueEx(key, "DisplayIcon", 0, self.winreg.REG_SZ, f'"{self.installed / "maple-agent.exe"}",0')

    def ready(self, timeout=20):
        if self.before is None:
            raise ValueError("Windows icon probe requires a legacy metadata fixture")
        with self.registration() as key:
            self.registered_root(key)
            value = self.winreg.QueryValueEx(key, "DisplayIcon")[0]
        # This is the worker's final actual metadata mutation. Old metadata is
        # ordinary asynchronous work in progress, not a failed test to retry.
        if not windows_icon_equal(value, self.icon):
            return False
        regular(self.icon)
        if self.icon.read_bytes() != self.artwork:
            raise ValueError("Windows integration icon differs from canonical profile artwork")
        actual = self.shortcuts(timeout=timeout)
        for name in ("main", "uninstall"):
            if not windows_icon_equal(actual[name]["icon_location"], self.icon):
                raise ValueError("Windows shortcut icon did not migrate to its persistent profile asset")
            if {key: value for key, value in actual[name].items() if key != "icon_location"} != {key: value for key, value in self.before[name].items() if key != "icon_location"}:
                raise ValueError("Windows icon refresh changed a non-icon shortcut property")
        return True


def windows_path_equal(left, right):
    return isinstance(left, str) and ntpath.normcase(ntpath.normpath(left)) == ntpath.normcase(ntpath.normpath(str(right)))


def _local_windows_shortcut_target(value):
    text = str(value).replace("/", "\\")
    plain = text[4:] if text.startswith("\\\\?\\") else text
    if not re.match(r"^[A-Za-z]:\\", plain):
        raise ValueError("Windows shortcut target is not an absolute local drive path")
    for name in plain[3:].split("\\"):
        if (not name or name in (".", "..") or name.endswith((".", " "))
                or re.search(r'[\x00-\x1f<>:"|?*]', name)
                or re.fullmatch(r"(?:CON|PRN|AUX|NUL|CONIN\$|CONOUT\$|COM[0-9¹²³]|LPT[0-9¹²³])", name.split(".", 1)[0], re.I)):
            raise ValueError("Windows shortcut target contains an ambiguous path component")
    return Path(text)


def _canonical_shortcut_targets(actual, expected):
    # Check BOTH ordinary paths/ancestors before canonicalization can follow
    # anything. Distinct hardlink names remain distinct canonical paths.
    regular(actual)
    regular(expected)
    return actual.resolve(strict=True), expected.resolve(strict=True)


def windows_shortcut_target_equal(actual, expected):
    paths = (_local_windows_shortcut_target(actual), _local_windows_shortcut_target(expected))
    canonical = _canonical_shortcut_targets(*paths)
    # GetFinalPathNameByHandle-backed strict resolution expands 8.3 aliases.
    # Admit only local DOS forms again, including canonical verbatim DOS paths.
    names = []
    for path in canonical:
        text = str(_local_windows_shortcut_target(path))
        names.append(text[4:] if text.startswith("\\\\?\\") else text)
    return windows_path_equal(*names)


def windows_icon_equal(value, expected):
    if not isinstance(value, str) or "," not in value:
        return False
    location, index = value.rsplit(",", 1)
    location = location.strip()
    if location.startswith('"') and location.endswith('"'):
        location = location[1:-1]
    return index.strip() == "0" and windows_path_equal(location, expected)


@contextmanager
def verification_scratch(prefix, *, cleanup_facts=None, cleanup_failure=None):
    temporary = tempfile.TemporaryDirectory(prefix=prefix, ignore_cleanup_errors=False)
    scratch = Path(temporary.name)
    primary = None
    try:
        scratch = scratch.resolve()
        yield scratch
    except BaseException as error:
        primary = error
        raise
    finally:
        started = time.monotonic_ns()
        cleanup_error = None
        finished = None
        try:
            temporary.cleanup()
            require_scratch_absent(scratch)
        except Exception as error:
            cleanup_error = error
            finished = time.monotonic_ns()
            if cleanup_failure is not None:
                try:
                    cleanup_failure(scratch, error)
                except BaseException as diagnostic_error:
                    print("Windows cleanup diagnostic unavailable: " + type(diagnostic_error).__name__, file=sys.stderr)
            print("Windows verification cleanup failure: " + type(error).__name__, file=sys.stderr)
            if primary is None:
                raise
        finally:
            finished = finished if finished is not None else time.monotonic_ns()
            facts = {"prefix": prefix, "cleanup_started_monotonic_ns": started,
                     "cleanup_finished_monotonic_ns": finished, "cleanup_duration_ns": finished - started,
                     "result": "failure" if cleanup_error else "success"}
            child_returned = (cleanup_facts or {}).get("child_returned_monotonic_ns")
            if type(child_returned) is int:
                facts["child_returned_monotonic_ns"] = child_returned
                facts["child_to_cleanup_ns"] = started - child_returned
            if cleanup_error is not None:
                facts.update(error=type(cleanup_error).__name__, winerror=getattr(cleanup_error, "winerror", None))
                try:
                    facts["path"] = str(Path(getattr(cleanup_error, "filename", None)).relative_to(scratch))[:256]
                except (TypeError, ValueError):
                    pass
            # Report only after the actual cleanup and strict absence probes.
            print("Windows verification cleanup: " + json.dumps(facts, sort_keys=True), flush=True)


@contextmanager
def pin_current_pointer(path):
    # The exact-current repeat must not write or replace this pointer. Specify
    # Windows sharing explicitly; Python's file mode does not define it.
    import ctypes
    from ctypes import wintypes

    regular(path)
    kernel = ctypes.WinDLL("kernel32", use_last_error=True)
    create = kernel.CreateFileW
    create.argtypes = (wintypes.LPCWSTR, wintypes.DWORD, wintypes.DWORD,
                       wintypes.LPVOID, wintypes.DWORD, wintypes.DWORD, wintypes.HANDLE)
    create.restype = wintypes.HANDLE
    close = kernel.CloseHandle
    close.argtypes, close.restype = (wintypes.HANDLE,), wintypes.BOOL
    # GENERIC_READ, FILE_SHARE_READ only, NULL security (noninheritable),
    # OPEN_EXISTING. Neither writes nor delete/rename access can be admitted.
    handle = create(str(path), 0x80000000, 1, None, 3, 0, None)
    if handle == ctypes.c_void_p(-1).value:
        raise ctypes.WinError(ctypes.get_last_error())
    primary = None
    try:
        yield
    except BaseException as error:
        primary = error
        raise
    finally:
        try:
            if not close(handle):
                raise ctypes.WinError(ctypes.get_last_error())
        except Exception as error:
            print("Windows pointer pin cleanup failure: " + type(error).__name__, file=sys.stderr)
            if primary is None:
                raise


def smoke_failure_evidence(phase, error, installed, generation_name, registry, access, winreg):
    """Presence/type evidence only; no profile/store data or arbitrary walks."""
    def state(path):
        try:
            metadata = path.lstat()
        except FileNotFoundError:
            return "absent"
        except OSError:
            return "unavailable"
        if stat.S_ISLNK(metadata.st_mode) or getattr(metadata, "st_file_attributes", 0) & 0x400:
            return "reparse"
        return "directory" if stat.S_ISDIR(metadata.st_mode) else "file" if stat.S_ISREG(metadata.st_mode) else "special"
    try:
        root = state(installed)
        paths = {"root": root}
        if root == "directory":
            for name in ("maple-agent.exe", "uninstall.exe", "current.json", ".install.lock", ".installer-intent.json", ".uninstaller-intent.json", "versions"):
                paths[name] = state(installed / name)
            if paths["versions"] == "directory":
                generation = installed / "versions" / generation_name
                paths["generation"] = state(generation)
                if paths["generation"] == "directory":
                    for name in ("maple-agent.exe", "build-info.json", "bundle-info.json"):
                        paths["generation/" + name] = state(generation / name)
        try:
            with winreg.OpenKey(winreg.HKEY_CURRENT_USER, registry, 0, access):
                registration = "present"
        except FileNotFoundError:
            registration = "absent"
        except OSError:
            registration = "unavailable"
        code = error.returncode if isinstance(error, subprocess.CalledProcessError) else None
        native = decode_activation_failure(code) if phase in ("install", "repeat-install", "interactive-install") else None
        installer_phase = NSIS_FAILURE_PHASES.get(code, "unknown") if phase in ("install", "repeat-install", "interactive-install", "uninstall") else None
        if native:
            installer_phase = "activation-child"
        evidence = {"phase": phase, "error": type(error).__name__, "exit_code": code,
                    "installer_phase": installer_phase,
                    "registration": registration, "owned_paths": paths}
        if native:
            evidence.update(native)
        print("Windows installer failure evidence: " + json.dumps(evidence, sort_keys=True), file=sys.stderr)
    except Exception:
        # Diagnostics must never replace the failure being diagnosed.
        print("Windows installer failure evidence unavailable", file=sys.stderr)


def smoke_phase_complete(name, arch, **facts):
    print("Windows smoke phase complete: " + json.dumps({"phase": name, "arch": arch, **facts}, sort_keys=True), flush=True)


def uninstaller_digest(path):
    regular(path)
    digest, size = hashlib.sha256(), 0
    with path.open("rb") as source:
        while block := source.read(64 * 1024):
            size += len(block)
            if size > 32 * 1024 * 1024:
                raise ValueError("Windows uninstaller exceeds its verified extraction bound")
            digest.update(block)
    if size == 0:
        raise ValueError("Windows uninstaller is empty")
    return digest.hexdigest()


def require_absent(path, message):
    # Path.exists() may translate access denial into False on modern Python.
    # Only an actual missing entry is evidence that an owned file was removed.
    try:
        path.lstat()
    except FileNotFoundError:
        return
    raise ValueError(message)


def require_scratch_absent(scratch):
    require_absent(scratch / "uninstaller-runner" / "uninstall.exe", "Auxiliary uninstaller survived scratch cleanup")
    require_absent(scratch / "uninstaller-runner", "Auxiliary runner directory survived scratch cleanup")
    require_absent(scratch, "Windows verification scratch survived cleanup")


def run_copied_uninstaller(uninstaller, installed, scratch, environment, unsigned, expected_sha256, arch, *, cleanup_facts=None):
    """Model NSIS relocation but synchronously wait for the actual uninstaller.

    https://nsis.sourceforge.io/Docs/AppendixD.html#D.1 documents copying the
    uninstaller and invoking that copy with _?=$INSTDIR to retain its exit code.
    The running image is then outside the installation being removed.
    """
    platform_name(arch)
    if not isinstance(expected_sha256, str) or not re.fullmatch(r"[0-9a-f]{64}", expected_sha256):
        raise ValueError("Windows uninstall smoke requires its verified source hash")
    if uninstaller != installed / "uninstall.exe" or installed.parent != scratch:
        raise ValueError("Windows uninstall smoke requires its owned installation root")
    phase = "source-verification"
    copied = scratch / "uninstaller-runner" / "uninstall.exe"
    result = None
    try:
        if uninstaller_digest(uninstaller) != expected_sha256:
            raise ValueError("Installed uninstaller differs from the verified installer")
        signature(uninstaller, unsigned)
        # scratch is this smoke's private, ordinary temporary directory. The
        # new child inherits its trusted ownership; no shared TEMP name is used.
        copied.parent.mkdir(mode=0o700)
        phase = "copy-verification"
        with uninstaller.open("rb") as source, copied.open("xb") as target:
            size = 0
            while block := source.read(64 * 1024):
                size += len(block)
                if size > 32 * 1024 * 1024:
                    raise ValueError("Windows uninstaller copy exceeds its verified extraction bound")
                target.write(block)
        if uninstaller_digest(copied) != expected_sha256:
            raise ValueError("Private copied uninstaller differs from its verified source")
        signature(copied, unsigned)
        if uninstaller_digest(copied) != expected_sha256:
            raise ValueError("Copied uninstaller changed during signature verification")
        smoke_phase_complete("uninstaller-copy-verification", arch, sha256=expected_sha256)
        phase = "copied-process"
        # _?= is last and deliberately unquoted, as required by NSIS. This copy
        # performs the work itself, without NSIS's detached second invocation.
        result = run(command_as_windows_args(f'"{copied}" /S _?={installed}'),
                     cwd=scratch, env=environment, timeout=120)
        returned = time.monotonic_ns()
        if cleanup_facts is not None:
            cleanup_facts["child_returned_monotonic_ns"] = returned
        result.check_returncode()
        phase = "original-retirement"
        require_absent(uninstaller, "NSIS did not remove the original installed uninstaller")
        phase = "post-child-copy-verification"
        if uninstaller_digest(copied) != expected_sha256:
            raise ValueError("Copied uninstaller changed while running")
        # The private auxiliary runner belongs to verification_scratch, just
        # like the other temporary verifier inputs. All product assertions run
        # before standard cleanup; strict auxiliary absence is checked afterward.
        smoke_phase_complete("uninstaller-child-complete", arch, sha256=expected_sha256,
                             exit_code=result.returncode, original_absent=True)
    except BaseException as error:
        facts = {"phase": phase, "error": type(error).__name__, "exit_code": getattr(result, "returncode", None)}
        for name, path in (("original", uninstaller), ("copy", copied)):
            try:
                status = path.lstat()
                attributes = getattr(status, "st_file_attributes", 0)
                facts[name] = {"present": True, "readonly": bool(attributes & 1), "reparse": bool(attributes & 0x400)}
            except FileNotFoundError:
                facts[name] = {"present": False}
            except OSError:
                facts[name] = {"state": "unavailable"}
        print("Windows uninstaller retirement failure: " + json.dumps(facts, sort_keys=True), file=sys.stderr)
        raise


def smoke_installer(installer, info, unsigned, build, expected_hashes, *, checker, uninstaller_sha256, arch="x86_64"):
    platform_name(arch)
    import winreg  # Only the explicit native smoke path loads the Windows API.

    registry = "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\" + info["bundle_id"]
    registry_access = winreg.KEY_READ | winreg.KEY_WOW64_32KEY
    try:
        with winreg.OpenKey(winreg.HKEY_CURRENT_USER, registry, 0, registry_access):
            raise ValueError("Refusing to smoke over an existing Agent installation")
    except FileNotFoundError:
        pass
    cleanup_facts = {}
    with verification_scratch("maple-windows-install-", cleanup_facts=cleanup_facts) as scratch:
        environment = smoke_environment(scratch)
        installed = scratch / "install with spaces"
        command = f'"{installer}" /S /D={installed}'
        generation_name = info["version"] + "-" + build
        phase, primary, installed_ok = "read-only-preflight", None, False
        executable = installed / "maple-agent.exe"
        uninstaller = installed / "uninstall.exe"
        generation = installed / "versions" / generation_name
        try:
            arguments = [checker, "--internal-windows-installer-preflight", installed]
            preflight = capture_preflight(arguments, environment)
            if not (unsigned and info["profile"] == "prod") and preflight.returncode:
                bounded_diagnostics(preflight.stdout)
                raise subprocess.CalledProcessError(preflight.returncode, preflight.args)
            if unsigned and info["profile"] == "prod" and preflight.returncode == 0:
                raise ValueError("Unsigned Prod checker did not refuse read-only preflight")
            phase = "install"
            if unsigned and info["profile"] == "prod":
                # Keep the actual NSIS refusal proof in addition to the direct
                # read-only probe; neither may enable unsigned Prod installs.
                result = subprocess.run(command_as_windows_args(command), env=environment, timeout=120)
                if result.returncode == 0:
                    raise ValueError("Unsigned Prod installer did not refuse before destination changes")
                require_absent(installed, "Unsigned Prod installer did not refuse before destination changes")
                try:
                    with winreg.OpenKey(winreg.HKEY_CURRENT_USER, registry, 0, registry_access):
                        raise ValueError("Unsigned Prod installer changed installation registration")
                except FileNotFoundError:
                    pass
                print("Verified unsigned Prod refusal before installed files, pointer or registration changes; no positive GUI/install proof")
                return
            run(command_as_windows_args(command), env=environment, timeout=120)
            installed_ok = True
            phase = "installed-verification"
            with winreg.OpenKey(winreg.HKEY_CURRENT_USER, registry, 0, registry_access) as key:
                if winreg.QueryValueEx(key, "InstallLocation")[0] != str(installed):
                    raise ValueError("Installer did not register the exact per-user installation")
            pointer_bytes = (installed / "current.json").read_bytes()
            if json.loads(pointer_bytes, object_pairs_hook=pe.unique_object) != {"format": "maple-agent-windows-current-v1", "generation": generation_name}:
                raise ValueError("Installed Windows current generation pointer differs")
            verify_payload(info["profile"], generation, info, unsigned, arch=arch)
            bundle_payload_inventory(generation)
            for name in BUNDLE_FILES:
                regular(generation / name)
                if hashlib.sha256((generation / name).read_bytes()).hexdigest() != expected_hashes[name]:
                    raise ValueError("Installed Windows generation differs from the verified updater payload")
            if executable.read_bytes() != (generation / "maple-agent.exe").read_bytes():
                raise ValueError("Freshly installed Windows bootstrap differs from its generation")
            signature(executable, unsigned)
            signature(uninstaller, unsigned)
            # A fresh silent install must not start any app/host process.
            quiet_installer_smoke(generation / "maple-agent.exe")
            smoke_phase_complete("installed-verification", arch)
            phase = "installed-cli"
            actual = run([executable, "--build-info"], env=environment, capture_output=True, text=True).stdout
            if json.loads(actual) != info:
                raise ValueError("Installed Windows binary reports another profile/source")
            run([executable, "--version"], env=environment, stdout=subprocess.DEVNULL)
            smoke_phase_complete("installed-cli", arch)
            for _ in range(2):
                result = subprocess.run([str(executable), "acp"], cwd=scratch, env=environment, stdin=subprocess.DEVNULL, capture_output=True, timeout=45)
                if result.returncode != 1 or not any(message in result.stderr for message in (
                    b"Maple needs local store setup. Open Maple to continue.",
                    b"Maple ACP requires sign-in.",
                )):
                    bounded_diagnostics(result.stderr)
                    raise ValueError("Installed Windows ACP did not preserve the signed-out guard")
            smoke_phase_complete("signed-out-acp", arch)
            # Bootstrap CLI probes above exercise native argv/stdio/status
            # forwarding. GUI bootstrap intentionally detaches, so observe
            # the exact selected installed generation for render/lifetime and
            # deterministic cleanup instead of the short-lived launcher PID.
            phase = "gui"
            integration = WindowsIntegrationProbe(info, installed, scratch, environment, winreg)
            integration.seed()
            gui_smoke(generation / "maple-agent.exe", scratch, environment, integration_ready=integration.ready)
            smoke_phase_complete("gui-first-render-and-icons", arch)
            # A repeated manual installer must retain the active generation
            # and stable bootstrap rather than rewriting a running payload.
            bootstrap_hash = hashlib.sha256(executable.read_bytes()).hexdigest()
            # ACP/GUI probes can leave their legitimate background host using
            # the same PE. Retain its exact identity so it is never mistaken
            # for an installer-launched GUI or stopped by UI-smoke cleanup.
            with smoke_application_baseline(generation / "maple-agent.exe") as (ui, existing):
                phase = "repeat-install"
                with pin_current_pointer(installed / "current.json"):
                    run(command_as_windows_args(command), env=environment, timeout=120)
                    if (installed / "current.json").read_bytes() != pointer_bytes or hashlib.sha256(executable.read_bytes()).hexdigest() != bootstrap_hash:
                        raise ValueError("Repeated Windows install changed stable bootstrap or active pointer")
                quiet_installer_smoke(generation / "maple-agent.exe", ui=ui, existing=existing)
                smoke_phase_complete("silent-repeat-install", arch)
                phase = "interactive-install"
                interactive_installer_smoke(installer, installed, generation / "maple-agent.exe", scratch, environment, ui=ui, existing=existing,
                                           before_close=integration.seed, integration_ready=integration.ready)
            if (installed / "current.json").read_bytes() != pointer_bytes or hashlib.sha256(executable.read_bytes()).hexdigest() != bootstrap_hash:
                raise ValueError("Interactive repeat changed stable bootstrap or active pointer")
            smoke_phase_complete("interactive-auto-launch-and-icons", arch)
        except BaseException as error:
            primary = error
            smoke_failure_evidence(phase, error, installed, generation_name, registry, registry_access, winreg)
            raise
        finally:
            if installed_ok:
                cleanup_phase = "host-stop"
                try:
                    if primary is None:
                        # A successful body must prove the real installed
                        # uninstaller ran; scratch deletion cannot stand in for it.
                        regular(executable)
                        regular(uninstaller)
                    if primary is None or executable.is_file():
                        run([executable, "host", "stop"], env=environment, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=45)
                        smoke_phase_complete("host-stop", arch)
                    if primary is None or uninstaller.is_file():
                        sentinel = installed / "user-created-file.txt"
                        sentinel.write_text("preserve me", encoding="utf-8")
                        state = scratch / "data" / info["data_namespace"] / "state-marker.txt"
                        state.parent.mkdir(parents=True, exist_ok=True)
                        state.write_text("preserve state", encoding="utf-8")
                        cleanup_phase = "uninstall"
                        run_copied_uninstaller(uninstaller, installed, scratch, environment, unsigned, uninstaller_sha256, arch,
                                               cleanup_facts=cleanup_facts)
                        require_absent(executable, "Uninstall did not remove the app")
                        require_absent(uninstaller, "Uninstall did not remove the original uninstaller")
                        if sentinel.read_text(encoding="utf-8") != "preserve me" or state.read_text(encoding="utf-8") != "preserve state":
                            raise ValueError("Uninstall did not remove the app while preserving user files and state")
                        for name in ("current.json", ".install.lock", ".installer-intent.json", ".uninstaller-intent.json", "versions"):
                            require_absent(installed / name, "Uninstall left owned generation or transaction files behind")
                        try:
                            with winreg.OpenKey(winreg.HKEY_CURRENT_USER, registry, 0, registry_access):
                                raise ValueError("Uninstall left the profile's registration behind")
                        except FileNotFoundError:
                            pass
                        smoke_phase_complete("uninstall-owned-files-and-state", arch)
                except Exception as error:
                    smoke_failure_evidence(cleanup_phase, error, installed, generation_name, registry, registry_access, winreg)
                    if primary is None:
                        raise
    require_scratch_absent(scratch)
    smoke_phase_complete("installer-smoke-complete", arch)
    print("Verified Windows install, identity, signed-out ACP, GUI first render, silent/repeat install, interactive auto-launch, persistent shell icons, host stop and state-preserving uninstall")


def command_as_windows_args(command):
    # subprocess accepts a raw CreateProcess command line on Windows. NSIS
    # consumes the unquoted trailing /D= or _?= path (which can contain spaces).
    # Do not round-trip that suffix through subprocess.list2cmdline.
    return command


def bounded_diagnostics(data):
    # Only this probe's empty private profile is logged. Remove terminal control
    # bytes and cap output; never dump parent environment or stored account data.
    data = re.sub(rb"\x1b\[[0-9;]*[A-Za-z]", b"", data[-8192:])
    safe = bytes(value for value in data if value in (9, 10, 13) or 32 <= value < 127)
    print("Isolated Windows startup diagnostics:\n" + safe.decode("ascii"), file=sys.stderr)


class WindowsSmokeUI:
    """Native UI driver; inspect all PIDs, operate only the owned installer/app."""

    def __init__(self):
        import ctypes
        from ctypes import wintypes

        self.ctypes, self.types = ctypes, wintypes
        self.kernel = ctypes.WinDLL("kernel32", use_last_error=True)
        self.user = ctypes.WinDLL("user32", use_last_error=True)
        self.psapi = ctypes.WinDLL("psapi", use_last_error=True)
        self.callback = ctypes.WINFUNCTYPE(wintypes.BOOL, wintypes.HWND, wintypes.LPARAM)
        declarations = (
            (self.kernel.OpenProcess, (wintypes.DWORD, wintypes.BOOL, wintypes.DWORD), wintypes.HANDLE),
            (self.kernel.QueryFullProcessImageNameW, (wintypes.HANDLE, wintypes.DWORD, wintypes.LPWSTR, ctypes.POINTER(wintypes.DWORD)), wintypes.BOOL),
            (self.kernel.GetProcessTimes, (wintypes.HANDLE, *([ctypes.POINTER(wintypes.FILETIME)] * 4)), wintypes.BOOL),
            (self.kernel.CloseHandle, (wintypes.HANDLE,), wintypes.BOOL),
            (self.kernel.WaitForSingleObject, (wintypes.HANDLE, wintypes.DWORD), wintypes.DWORD),
            (self.kernel.TerminateProcess, (wintypes.HANDLE, wintypes.UINT), wintypes.BOOL),
            (self.psapi.EnumProcesses, (ctypes.POINTER(wintypes.DWORD), wintypes.DWORD, ctypes.POINTER(wintypes.DWORD)), wintypes.BOOL),
            (self.user.EnumWindows, (self.callback, wintypes.LPARAM), wintypes.BOOL),
            (self.user.GetWindowThreadProcessId, (wintypes.HWND, ctypes.POINTER(wintypes.DWORD)), wintypes.DWORD),
            (self.user.IsWindowVisible, (wintypes.HWND,), wintypes.BOOL),
            (self.user.IsWindowEnabled, (wintypes.HWND,), wintypes.BOOL),
            (self.user.GetDlgItem, (wintypes.HWND, ctypes.c_int), wintypes.HWND),
            (self.user.GetWindowTextW, (wintypes.HWND, wintypes.LPWSTR, ctypes.c_int), ctypes.c_int),
            (self.user.PostMessageW, (wintypes.HWND, wintypes.UINT, wintypes.WPARAM, wintypes.LPARAM), wintypes.BOOL),
        )
        for function, arguments, result in declarations:
            function.argtypes, function.restype = arguments, result

    def windows(self, pid):
        windows = []
        @self.callback
        def collect(window, _data):
            owner = self.types.DWORD()
            self.user.GetWindowThreadProcessId(window, self.ctypes.byref(owner))
            if owner.value == pid:
                windows.append(window)
            return True
        if not self.user.EnumWindows(collect, 0):
            raise self.ctypes.WinError(self.ctypes.get_last_error())
        return windows

    def button(self, pid, caption):
        for window in self.windows(pid):
            button = self.user.GetDlgItem(window, 1)  # Stock NSIS Next/Install/Close.
            text = self.ctypes.create_unicode_buffer(128)
            if button and self.user.IsWindowEnabled(window) and self.user.IsWindowEnabled(button):
                self.user.GetWindowTextW(button, text, len(text))
                if text.value.replace("&", "") == caption:
                    return window, button
        return None

    def click(self, button):
        # WM_COMMAND / BN_CLICKED to this owned dialog; no global input or focus.
        if not self.user.PostMessageW(button[0], 0x111, 1, button[1]):
            raise self.ctypes.WinError(self.ctypes.get_last_error())

    def applications(self, executable):
        pids = (self.types.DWORD * 4096)()
        used = self.types.DWORD()
        if not self.psapi.EnumProcesses(pids, self.ctypes.sizeof(pids), self.ctypes.byref(used)):
            raise self.ctypes.WinError(self.ctypes.get_last_error())
        if used.value >= self.ctypes.sizeof(pids):
            raise ValueError("Windows smoke process inventory exceeded its bounds")
        expected = os.path.normcase(os.path.normpath(str(executable)))
        matches = []
        for pid in pids[:used.value // self.ctypes.sizeof(self.types.DWORD)]:
            # Query + synchronize + terminate. A retained native handle prevents
            # cleanup from targeting a different process after PID reuse.
            handle = self.kernel.OpenProcess(0x1000 | 0x100000 | 1, False, pid)
            if not handle:
                continue  # Other users/system processes are outside this smoke.
            image = self.ctypes.create_unicode_buffer(32768)
            size = self.types.DWORD(len(image))
            matched = self.kernel.QueryFullProcessImageNameW(handle, 0, image, self.ctypes.byref(size)) and os.path.normcase(os.path.normpath(image.value)) == expected
            if matched:
                try:
                    matches.append(WindowsSmokeApplication(self, handle, pid))
                except BaseException:
                    for process in matches:
                        process.release()
                    raise
            else:
                self.kernel.CloseHandle(handle)
        return matches

    def application(self, executable, *, existing=(), visible_only=False):
        selected = None
        candidates = self.applications(executable)
        try:
            for candidate in candidates:
                if selected is None and candidate.identity not in existing and (not visible_only or candidate.visible()):
                    selected = candidate
            return selected
        finally:
            for candidate in candidates:
                if candidate is not selected:
                    candidate.release()


class WindowsSmokeApplication:
    def __init__(self, ui, handle, pid):
        self.ui, self.handle, self.pid = ui, handle, pid
        created, exited, kernel, user = (ui.types.FILETIME() for _ in range(4))
        if not ui.kernel.GetProcessTimes(handle, *(ui.ctypes.byref(value) for value in (created, exited, kernel, user))):
            error = ui.ctypes.WinError(ui.ctypes.get_last_error())
            self.release()
            raise error
        self.identity = pid, (created.dwHighDateTime << 32) | created.dwLowDateTime

    def alive(self):
        result = self.ui.kernel.WaitForSingleObject(self.handle, 0)
        if result not in (0, 258):
            raise self.ui.ctypes.WinError(self.ui.ctypes.get_last_error())
        return result == 258  # WAIT_TIMEOUT means this exact process is live.

    def visible(self):
        return any(self.ui.user.IsWindowVisible(window) for window in self.ui.windows(self.pid))

    def close(self):
        try:
            if self.alive():
                if not self.ui.kernel.TerminateProcess(self.handle, 1):
                    error = self.ui.ctypes.WinError(self.ui.ctypes.get_last_error())
                    # A natural exit between the liveness probe and terminate
                    # is harmless only when this same handle is signalled.
                    if self.ui.kernel.WaitForSingleObject(self.handle, 0) != 0:
                        raise error
                if self.ui.kernel.WaitForSingleObject(self.handle, 10000) != 0:
                    raise ValueError("Owned Windows smoke application did not stop")
        finally:
            self.release()

    def release(self):
        self.ui.kernel.CloseHandle(self.handle)


@contextmanager
def smoke_application_baseline(executable):
    ui = WindowsSmokeUI()
    processes = ui.applications(executable)
    try:
        yield ui, {process.identity for process in processes}
    finally:
        for process in processes:
            process.release()  # Existing hosts remain owned by host-stop below.


def quiet_installer_smoke(executable, *, ui=None, existing=()):
    ui = ui or WindowsSmokeUI()
    deadline = time.monotonic() + 1
    while time.monotonic() < deadline:
        unexpected, cleanup_error = False, None
        for application in ui.applications(executable):
            try:
                if application.identity in existing:
                    application.release()
                else:
                    unexpected = True
                    application.close()
            except Exception as error:
                cleanup_error = cleanup_error or error
        if unexpected:
            if cleanup_error:
                print("Windows silent smoke cleanup failure: " + type(cleanup_error).__name__, file=sys.stderr)
            raise ValueError("Silent Windows installer launched the application")
        if cleanup_error:
            raise cleanup_error
        time.sleep(0.05)


def interactive_installer_smoke(installer, installed, executable, scratch, environment, *, ui=None, existing=(), before_close=None, integration_ready=None):
    ui = ui or WindowsSmokeUI()
    command = command_as_windows_args(f'"{installer}" /D={installed}')
    process = subprocess.Popen(command, cwd=scratch, env=environment, stdin=subprocess.DEVNULL,
                               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    application, primary = None, None
    try:
        deadline = time.monotonic() + 120
        for caption in ("Install", "Close"):
            while time.monotonic() < deadline:
                if process.poll() is not None:
                    if process.returncode:
                        raise subprocess.CalledProcessError(process.returncode, command)
                    raise ValueError("Interactive Windows installer exited before completion")
                button = ui.button(process.pid, caption)
                if button:
                    if caption == "Close":
                        application = ui.application(executable, existing=existing)
                        if application:
                            raise ValueError("Interactive Windows installer launched before it closed")
                        if before_close:
                            before_close(max(0.1, deadline - time.monotonic()))
                    ui.click(button)
                    break
                time.sleep(0.05)
            else:
                raise ValueError("Interactive Windows installer did not reach " + caption)
        code = process.wait(timeout=max(1, deadline - time.monotonic()))
        if code:
            raise subprocess.CalledProcessError(code, command)
        # Observe the process launched by NSIS through the verified bootstrap,
        # rather than launching the generation ourselves or trusting its PID.
        deadline, ready_at = time.monotonic() + 45, None
        integration_pending = False
        while time.monotonic() < deadline:
            application = application or ui.application(executable, existing=existing, visible_only=True)
            if application:
                if not application.alive():
                    raise ValueError("Auto-launched Windows application exited early")
                if application.visible():
                    ready_at = ready_at or time.monotonic()
                    if time.monotonic() - ready_at >= 1:
                        if integration_ready is None:
                            return
                        if integration_ready(max(0.1, deadline - time.monotonic())):
                            if not application.alive() or not application.visible():
                                raise ValueError("Auto-launched Windows GUI exited or closed during icon verification")
                            return
                        integration_pending = True
            time.sleep(0.05)
        if integration_pending:
            raise ValueError("Windows shell icon refresh did not finish before the GUI smoke deadline")
        raise ValueError("Interactive Windows installer did not open the installed application")
    except BaseException as error:
        primary = error
        raise
    finally:
        # An installer cleanup failure must not prevent exact-app cleanup, or
        # replace the original failure from the interactive proof.
        cleanup_error = None
        try:
            if process.poll() is None:
                process.terminate()
            process.wait(timeout=10)
        except Exception as error:
            cleanup_error = error
        attempted = set()
        try:
            if application:
                attempted.add(application.identity)
                application.close()
        except Exception as error:
            cleanup_error = cleanup_error or error
        if primary is not None:
            # A failed/hidden launch can leave more than one new process. On
            # failure collect all of this isolated generation's new processes;
            # success leaves its legitimate host for the host-stop proof.
            try:
                candidates = ui.applications(executable)
            except Exception as error:
                candidates = []
                cleanup_error = cleanup_error or error
            for candidate in candidates:
                try:
                    if candidate.identity in existing or candidate.identity in attempted:
                        candidate.release()
                    else:
                        candidate.close()
                except Exception as error:
                    cleanup_error = cleanup_error or error
        if cleanup_error:
            print("Windows interactive smoke cleanup failure: " + type(cleanup_error).__name__, file=sys.stderr)
            if primary is None:
                raise cleanup_error


def gui_smoke(executable, scratch, environment, *, integration_ready=None):
    output = scratch / "gui.log"
    with output.open("wb") as stream:
        process = subprocess.Popen([str(executable)], cwd=scratch, env=environment, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=stream)
        try:
            deadline = time.monotonic() + 45
            ready_at = None
            integration_pending = False
            while time.monotonic() < deadline:
                if process.poll() is not None:
                    raise ValueError("Installed Windows GUI exited before first render")
                if output.stat().st_size > 2 * 1024 * 1024:
                    raise ValueError("Installed Windows GUI exceeded diagnostic bounds")
                diagnostics = output.read_bytes()
                if b"startup: window open at " in diagnostics and b"startup: first render at " in diagnostics:
                    ready_at = ready_at or time.monotonic()
                    if time.monotonic() - ready_at >= 1:
                        if integration_ready is None:
                            return
                        if integration_ready(max(0.1, deadline - time.monotonic())):
                            if process.poll() is not None:
                                raise ValueError("Installed Windows GUI exited during icon verification")
                            return
                        integration_pending = True
                time.sleep(0.05)
            if integration_pending:
                raise ValueError("Windows shell icon refresh did not finish before the GUI smoke deadline")
            raise ValueError("Installed Windows GUI did not create and render its window")
        except (ValueError, OSError, subprocess.SubprocessError):
            bounded_diagnostics(output.read_bytes()[-8192:])
            raise
        finally:
            if process.poll() is None:
                process.terminate()
            process.wait(timeout=10)


def verify(profile, directory, unsigned, *, smoke, arch="x86_64"):
    platform = platform_name(arch)
    directory = directory.resolve()
    manifest = release_info.verify_artifacts(profile, directory, unsigned, source_sha(), platform)
    if manifest["build"] != build_number():
        raise ValueError("Windows package build differs from the selected producer build")
    info = release_info.read_info(profile, directory / "build-info.json", source_sha(), platform)
    installer = directory / manifest["package"]
    signature(installer, unsigned)
    with verification_scratch("maple-windows-extract-") as payload:
        uninstaller = extract_installer(installer, payload, unsigned, info, manifest["build"])
        generation = payload / "versions" / (info["version"] + "-" + manifest["build"])
        verify_payload(profile, generation, info, unsigned, arch=arch)
        signature(uninstaller, unsigned)
        expected_hashes = inspect_bundle(directory / manifest["updater"], info, manifest["build"], generation)
        if smoke:
            require_native_host(arch)
            if any(name.startswith(("AZURE_", "MAPLE_WINDOWS_ARTIFACT_SIGNING_")) for name in os.environ):
                raise ValueError("Installed Windows smoke requires a verifier without signing configuration")
            smoke_installer(installer, info, unsigned, manifest["build"], expected_hashes,
                            checker=payload / INSTALLER_CHECKER, uninstaller_sha256=uninstaller_digest(uninstaller), arch=arch)
    print(f"Verified {profile} {platform} installer identity, exact inventory and signatures")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("operation", choices=("prepare", "package", "verify"))
    parser.add_argument("profile", choices=("dev", "prod"))
    parser.add_argument("directory", type=Path, nargs="?")
    parser.add_argument("--unsigned", action="store_true")
    parser.add_argument("--smoke", action="store_true")
    parser.add_argument("--arch", choices=WINDOWS_ARCHITECTURES, default="x86_64")
    args = parser.parse_args()
    if sys.platform != "win32":
        raise ValueError("Windows installer packaging and verification require native Windows")
    if args.operation == "prepare":
        prepare(args.profile, args.arch)
    elif args.operation == "package":
        package(args.profile, args.unsigned, args.arch)
    elif args.directory is None:
        parser.error("verify requires an artifact directory")
    else:
        verify(args.profile, args.directory, args.unsigned, smoke=args.smoke, arch=args.arch)


if __name__ == "__main__":
    try:
        main()
    except (ValueError, OSError, subprocess.SubprocessError, zipfile.BadZipFile, tarfile.TarError) as error:
        print(f"Windows package verification failed: {error}", file=sys.stderr)
        sys.exit(1)
