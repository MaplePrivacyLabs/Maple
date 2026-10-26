#!/usr/bin/env python3
"""Package and verify native Agent Windows installers, separately from builds.

The package path treats the prebuilt executable as data, even before signing.
Only an explicit --smoke on a credential-free Windows verifier runs the app.
"""

import argparse
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
import tempfile
import threading
import time
import urllib.request
import zipfile


SCRIPTS = Path(__file__).resolve().parent
COMPONENT = SCRIPTS.parent
TARGET = "x86_64-pc-windows-msvc"
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
NOTICE_FILES = (
    "THIRD_PARTY_NOTICES.txt", "THIRD_PARTY_NOTICES.html", "manifest.json",
    "wordlists/eff-large-wordlist.txt", "wordlists/SOURCES.md",
)
PAYLOAD_FILES = {"maple-agent.exe", "LICENSE.txt", "build-info.json"} | {
    f"notices/{name}" for name in NOTICE_FILES
}


def module(name):
    spec = importlib.util.spec_from_file_location(name.replace("-", "_"), SCRIPTS / f"{name}.py")
    loaded = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(loaded)
    return loaded


release_info = module("release-info")
pe = module("windows-build-info")
notices = module("stage-third-party-notices")


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
    notices.validate(COMPONENT, notices.output_directory(COMPONENT, TARGET), TARGET)
    print("Verified exact Windows binary profile, source, DLL closure and notice inputs")
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


def write_nsis_config(path, info, payload, installer, unsigned):
    values = {
        "DISPLAY_NAME": info["display_name"], "BUNDLE_ID": info["bundle_id"],
        "VERSION": info["version"], "NUMERIC_VERSION": info["version"].split("-", 1)[0],
        "PAYLOAD": payload, "INSTALLER": installer,
    }
    if not unsigned:
        signer = SCRIPTS / "windows-sign-file.ps1"
        # !uninstfinalize is a compiler command string. Reject cmd.exe expansion
        # syntax in the trusted script's filesystem path rather than quoting it
        # as though a JSON string or argv list controlled cmd.exe parsing.
        if any(char in str(signer) for char in '%&|<>^"\r\n'):
            raise ValueError("Unsafe path for the native uninstaller signing command")
        values["SIGNER"] = signer
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


def extract_installer(installer, destination, unsigned):
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
    # Depending on the 7-Zip version, an unsigned NSIS uninstaller is either
    # reconstructed as a PE or exposed as bytecode with a .nsis suffix. Signed
    # builds must contain the actual signed standalone PE from !uninstfinalize.
    extras = set(files) - PAYLOAD_FILES
    allowed_uninstallers = {"uninstall.exe", "uninstall.exe.nsis"} if unsigned else {"uninstall.exe"}
    if len(extras) != 1 or not extras <= allowed_uninstallers or not PAYLOAD_FILES <= set(files):
        raise ValueError("Windows installer has an unexpected payload inventory")
    uninstaller = next(iter(extras))
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
    return destination / uninstaller


def verify_payload(profile, directory, expected_info, unsigned, *, verify_signature=True):
    binary = directory / "maple-agent.exe"
    info = pe.validated_build_info(profile, binary, source_sha())
    if info != expected_info or json.loads((directory / "build-info.json").read_text(encoding="utf-8")) != info:
        raise ValueError("Installer payload identity differs from its verified build")
    validate_imports(binary)
    notices.validate(COMPONENT, directory / "notices", TARGET)
    for name, original in (("LICENSE.txt", COMPONENT / "LICENSE"), ("notices/wordlists/eff-large-wordlist.txt", COMPONENT / "crates/maple-agent/src/store/keys/eff-large-wordlist.txt"), ("notices/wordlists/SOURCES.md", COMPONENT / "crates/maple-agent/src/store/keys/SOURCES.md")):
        if (directory / name).read_bytes() != original.read_bytes():
            raise ValueError("Installer license or wordlist differs from the reviewed source")
    if verify_signature:
        signature(binary, unsigned)


def package(profile, unsigned):
    binary, info = prepare(profile)
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
        notices.stage(COMPONENT, payload / "notices", TARGET)
        signature(payload / "maple-agent.exe", unsigned, sign=not unsigned)
        verify_payload(profile, payload, info, unsigned)
        installer = output / release_info.windows_package_name(info)
        configuration = scratch / "package.nsh"
        write_nsis_config(configuration, info, payload, installer, unsigned)
        compiler = install_nsis(scratch)
        run([compiler, "/V3", f"/DMAPLE_CONFIG={configuration}", SCRIPTS / "windows-installer.nsi"], timeout=300)
        signature(installer, unsigned, sign=not unsigned)
        shutil.copyfile(payload / "build-info.json", output / "build-info.json")
        release_info.write_manifest(info, output, PLATFORM, unsigned)
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


def smoke_installer(installer, info, unsigned):
    import winreg  # Only the explicit native smoke path loads the Windows API.

    registry = "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\" + info["bundle_id"]
    registry_access = winreg.KEY_READ | winreg.KEY_WOW64_32KEY
    try:
        with winreg.OpenKey(winreg.HKEY_CURRENT_USER, registry, 0, registry_access):
            raise ValueError("Refusing to smoke over an existing Agent installation")
    except FileNotFoundError:
        pass
    with tempfile.TemporaryDirectory(prefix="maple-windows-install-") as temporary:
        scratch = Path(temporary).resolve()
        environment = smoke_environment(scratch)
        installed = scratch / "install with spaces"
        command = f'"{installer}" /S /D={installed}'
        run(command_as_windows_args(command), env=environment, timeout=120)
        executable = installed / "maple-agent.exe"
        uninstaller = installed / "uninstall.exe"
        try:
            with winreg.OpenKey(winreg.HKEY_CURRENT_USER, registry, 0, registry_access) as key:
                if winreg.QueryValueEx(key, "InstallLocation")[0] != str(installed):
                    raise ValueError("Installer did not register the exact per-user installation")
            verify_payload(info["profile"], installed, info, unsigned)
            signature(uninstaller, unsigned)
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
            gui_smoke(executable, scratch, environment)
        finally:
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
                run(command_as_windows_args(f'"{uninstaller}" /S _?={installed}'), env=environment, timeout=120)
                if executable.exists() or sentinel.read_text(encoding="utf-8") != "preserve me" or state.read_text(encoding="utf-8") != "preserve state":
                    raise ValueError("Uninstall did not remove the app while preserving user files and state")
                try:
                    with winreg.OpenKey(winreg.HKEY_CURRENT_USER, registry, 0, registry_access):
                        raise ValueError("Uninstall left the profile's registration behind")
                except FileNotFoundError:
                    pass
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
    info = release_info.read_info(profile, directory / "build-info.json", source_sha(), PLATFORM)
    installer = directory / manifest["package"]
    signature(installer, unsigned)
    with tempfile.TemporaryDirectory(prefix="maple-windows-extract-") as temporary:
        payload = Path(temporary)
        uninstaller = extract_installer(installer, payload, unsigned)
        verify_payload(profile, payload, info, unsigned)
        if not unsigned:
            signature(uninstaller, unsigned=False)
    if smoke:
        if any(name.startswith(("AZURE_", "MAPLE_WINDOWS_ARTIFACT_SIGNING_")) for name in os.environ):
            raise ValueError("Installed Windows smoke requires a verifier without signing configuration")
        smoke_installer(installer, info, unsigned)
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
    except (ValueError, OSError, subprocess.SubprocessError, zipfile.BadZipFile) as error:
        print(f"Windows package verification failed: {error}", file=sys.stderr)
        sys.exit(1)
