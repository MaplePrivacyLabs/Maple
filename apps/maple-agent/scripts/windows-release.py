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
import zipfile


SCRIPTS = Path(__file__).resolve().parent
COMPONENT = SCRIPTS.parent
PLATFORM = "windows-x86_64"
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


def prepare(profile):
    binary = COMPONENT / "target/release/maple-agent.exe"
    regular(binary)
    info = pe.validated_build_info(profile, binary, source_sha())
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


def write_nsis_config(path, info, payload, installer, unsigned, build):
    values = {
        "DISPLAY_NAME": info["display_name"], "BUNDLE_ID": info["bundle_id"],
        "VERSION": info["version"], "NUMERIC_VERSION": info["version"].split("-", 1)[0],
        "PAYLOAD": payload, "INSTALLER": installer,
        "BUILD": build, "GENERATION": info["version"] + "-" + build,
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


def verify_payload(profile, directory, expected_info, unsigned, *, verify_signature=True):
    binary = directory / "maple-agent.exe"
    info = pe.validated_build_info(profile, binary, source_sha())
    if info != expected_info or json.loads((directory / "build-info.json").read_text(encoding="utf-8")) != info:
        raise ValueError("Installer payload identity differs from its verified build")
    validate_imports(binary)
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


def package(profile, unsigned):
    binary, info = prepare(profile)
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
        verify_payload(profile, payload, info, unsigned)
        installer = output / release_info.windows_package_name(info)
        configuration = scratch / "package.nsh"
        write_nsis_config(configuration, info, payload, installer, unsigned, build)
        compiler = install_nsis(scratch)
        run([compiler, "/V3", f"/DMAPLE_CONFIG={configuration}", SCRIPTS / "windows-installer.nsi"], timeout=300)
        signature(installer, unsigned, sign=not unsigned)
        extracted = scratch / "extracted"
        extracted.mkdir()
        uninstaller = extract_installer(installer, extracted, unsigned, info, build)
        if uninstaller.name != "uninstall.exe":
            raise ValueError("Windows updater requires a standalone PE uninstaller")
        generation = extracted / "versions" / (info["version"] + "-" + build)
        verify_payload(profile, generation, info, unsigned)
        signature(uninstaller, unsigned)
        write_bundle(generation, output / release_info.windows_archive_name(info), info, build)
        shutil.copyfile(payload / "build-info.json", output / "build-info.json")
        release_info.write_manifest(info, output, PLATFORM, unsigned, build)
        # Data-only parsing of the compressed final signed bytes; never execute
        # the app, generated uninstaller or installer on the signing worker.
        verify(profile, output, unsigned, smoke=False)
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
    """Read-only checker only: bound combined output before retaining it."""
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


@contextmanager
def verification_scratch(prefix):
    temporary = tempfile.TemporaryDirectory(prefix=prefix)
    primary = None
    try:
        yield Path(temporary.name).resolve()
    except BaseException as error:
        primary = error
        raise
    finally:
        try:
            temporary.cleanup()
        except Exception as error:
            print("Windows verification cleanup failure: " + type(error).__name__, file=sys.stderr)
            if primary is None:
                raise


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
        native = decode_activation_failure(code) if phase in ("install", "repeat-install") else None
        installer_phase = NSIS_FAILURE_PHASES.get(code, "unknown") if phase in ("install", "repeat-install", "uninstall") else None
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


def smoke_installer(installer, info, unsigned, build, expected_hashes, *, checker):
    import winreg  # Only the explicit native smoke path loads the Windows API.

    registry = "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\" + info["bundle_id"]
    registry_access = winreg.KEY_READ | winreg.KEY_WOW64_32KEY
    try:
        with winreg.OpenKey(winreg.HKEY_CURRENT_USER, registry, 0, registry_access):
            raise ValueError("Refusing to smoke over an existing Agent installation")
    except FileNotFoundError:
        pass
    with verification_scratch("maple-windows-install-") as scratch:
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
                if result.returncode == 0 or installed.exists():
                    raise ValueError("Unsigned Prod installer did not refuse before destination changes")
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
            verify_payload(info["profile"], generation, info, unsigned)
            bundle_payload_inventory(generation)
            for name in BUNDLE_FILES:
                regular(generation / name)
                if hashlib.sha256((generation / name).read_bytes()).hexdigest() != expected_hashes[name]:
                    raise ValueError("Installed Windows generation differs from the verified updater payload")
            if executable.read_bytes() != (generation / "maple-agent.exe").read_bytes():
                raise ValueError("Freshly installed Windows bootstrap differs from its generation")
            signature(executable, unsigned)
            signature(uninstaller, unsigned)
            phase = "installed-cli"
            actual = run([executable, "--build-info"], env=environment, capture_output=True, text=True).stdout
            if json.loads(actual) != info:
                raise ValueError("Installed Windows binary reports another profile/source")
            run([executable, "--version"], env=environment, stdout=subprocess.DEVNULL)
            for _ in range(2):
                result = subprocess.run([str(executable), "acp"], cwd=scratch, env=environment, stdin=subprocess.DEVNULL, capture_output=True, timeout=45)
                if result.returncode != 1 or not any(message in result.stderr for message in (
                    b"Maple needs local store setup. Open Maple to continue.",
                    b"Maple ACP requires sign-in.",
                )):
                    bounded_diagnostics(result.stderr)
                    raise ValueError("Installed Windows ACP did not preserve the signed-out guard")
            # Bootstrap CLI probes above exercise native argv/stdio/status
            # forwarding. GUI bootstrap intentionally detaches, so observe
            # the exact selected installed generation for render/lifetime and
            # deterministic cleanup instead of the short-lived launcher PID.
            phase = "gui"
            gui_smoke(generation / "maple-agent.exe", scratch, environment)
            # A repeated manual installer must retain the active generation
            # and stable bootstrap rather than rewriting a running payload.
            bootstrap_hash = hashlib.sha256(executable.read_bytes()).hexdigest()
            phase = "repeat-install"
            with pin_current_pointer(installed / "current.json"):
                run(command_as_windows_args(command), env=environment, timeout=120)
                if (installed / "current.json").read_bytes() != pointer_bytes or hashlib.sha256(executable.read_bytes()).hexdigest() != bootstrap_hash:
                    raise ValueError("Repeated Windows install changed stable bootstrap or active pointer")
        except BaseException as error:
            primary = error
            smoke_failure_evidence(phase, error, installed, generation_name, registry, registry_access, winreg)
            raise
        finally:
            if installed_ok:
                cleanup_phase = "host-stop"
                try:
                    if executable.is_file():
                        run([executable, "host", "stop"], env=environment, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=45)
                    if uninstaller.is_file():
                        sentinel = installed / "user-created-file.txt"
                        sentinel.write_text("preserve me", encoding="utf-8")
                        state = scratch / "data" / info["data_namespace"] / "state-marker.txt"
                        state.parent.mkdir(parents=True, exist_ok=True)
                        state.write_text("preserve state", encoding="utf-8")
                        # _?= suppresses the uninstaller's detached temp copy, so the
                        # process exit really completes the cleanup being checked.
                        cleanup_phase = "uninstall"
                        run(command_as_windows_args(f'"{uninstaller}" /S _?={installed}'), env=environment, timeout=120)
                        if executable.exists() or sentinel.read_text(encoding="utf-8") != "preserve me" or state.read_text(encoding="utf-8") != "preserve state":
                            raise ValueError("Uninstall did not remove the app while preserving user files and state")
                        if any((installed / name).exists() for name in ("current.json", ".install.lock", ".installer-intent.json", ".uninstaller-intent.json", "versions")):
                            raise ValueError("Uninstall left owned generation or transaction files behind")
                        try:
                            with winreg.OpenKey(winreg.HKEY_CURRENT_USER, registry, 0, registry_access):
                                raise ValueError("Uninstall left the profile's registration behind")
                        except FileNotFoundError:
                            pass
                except Exception as error:
                    smoke_failure_evidence(cleanup_phase, error, installed, generation_name, registry, registry_access, winreg)
                    if primary is None:
                        raise
    print("Verified Windows install, identity, signed-out ACP, GUI first render, host stop and state-preserving uninstall")


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


def gui_smoke(executable, scratch, environment):
    output = scratch / "gui.log"
    with output.open("wb") as stream:
        process = subprocess.Popen([str(executable)], cwd=scratch, env=environment, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=stream)
        try:
            deadline = time.monotonic() + 45
            ready_at = None
            while time.monotonic() < deadline:
                if process.poll() is not None:
                    raise ValueError("Installed Windows GUI exited before first render")
                if output.stat().st_size > 2 * 1024 * 1024:
                    raise ValueError("Installed Windows GUI exceeded diagnostic bounds")
                diagnostics = output.read_bytes()
                if b"startup: window open at " in diagnostics and b"startup: first render at " in diagnostics:
                    ready_at = ready_at or time.monotonic()
                    if time.monotonic() - ready_at >= 1:
                        return
                time.sleep(0.05)
            raise ValueError("Installed Windows GUI did not create and render its window")
        except (ValueError, OSError, subprocess.SubprocessError):
            bounded_diagnostics(output.read_bytes()[-8192:])
            raise
        finally:
            if process.poll() is None:
                process.terminate()
            process.wait(timeout=10)


def verify(profile, directory, unsigned, *, smoke):
    directory = directory.resolve()
    manifest = release_info.verify_artifacts(profile, directory, unsigned, source_sha(), PLATFORM)
    if manifest["build"] != build_number():
        raise ValueError("Windows package build differs from the selected producer build")
    info = release_info.read_info(profile, directory / "build-info.json", source_sha(), PLATFORM)
    installer = directory / manifest["package"]
    signature(installer, unsigned)
    with verification_scratch("maple-windows-extract-") as payload:
        uninstaller = extract_installer(installer, payload, unsigned, info, manifest["build"])
        generation = payload / "versions" / (info["version"] + "-" + manifest["build"])
        verify_payload(profile, generation, info, unsigned)
        signature(uninstaller, unsigned)
        expected_hashes = inspect_bundle(directory / manifest["updater"], info, manifest["build"], generation)
        if smoke:
            if any(name.startswith(("AZURE_", "MAPLE_WINDOWS_ARTIFACT_SIGNING_")) for name in os.environ):
                raise ValueError("Installed Windows smoke requires a verifier without signing configuration")
            smoke_installer(installer, info, unsigned, manifest["build"], expected_hashes,
                            checker=payload / INSTALLER_CHECKER)
    print(f"Verified {profile} Windows installer identity, exact inventory and signatures")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("operation", choices=("prepare", "package", "verify"))
    parser.add_argument("profile", choices=("dev", "prod"))
    parser.add_argument("directory", type=Path, nargs="?")
    parser.add_argument("--unsigned", action="store_true")
    parser.add_argument("--smoke", action="store_true")
    args = parser.parse_args()
    if sys.platform != "win32":
        raise ValueError("Windows installer packaging and verification require native Windows")
    if args.operation == "prepare":
        prepare(args.profile)
    elif args.operation == "package":
        package(args.profile, args.unsigned)
    elif args.directory is None:
        parser.error("verify requires an artifact directory")
    else:
        verify(args.profile, args.directory, args.unsigned, smoke=args.smoke)


if __name__ == "__main__":
    try:
        main()
    except (ValueError, OSError, subprocess.SubprocessError, zipfile.BadZipFile, tarfile.TarError) as error:
        print(f"Windows package verification failed: {error}", file=sys.stderr)
        sys.exit(1)
