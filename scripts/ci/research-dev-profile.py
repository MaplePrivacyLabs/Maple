#!/usr/bin/env python3
"""Fixed local Research Dev packaging inputs and artifact checks."""
import argparse
import configparser
import copy
import hashlib
import json
import os
from pathlib import Path
import plistlib
import re
import shlex
import subprocess

ROOT = Path(__file__).resolve().parents[2]
TAURI = ROOT / "apps/maple-research/frontend/src-tauri"
PROFILE = TAURI / "desktop-dev-profile.json"


def validate_config(profile, overlay):
    if overlay["identifier"] != profile["identifier"] or overlay["productName"] != profile["productName"]:
        raise ValueError("Dev package identity does not match its profile")
    if overlay["plugins"]["deep-link"]["desktop"]["schemes"] != [profile["scheme"]]:
        raise ValueError("Dev package must own only its Dev scheme")
    if overlay["plugins"]["updater"]["endpoints"] != [] or overlay["bundle"]["createUpdaterArtifacts"] is not False:
        raise ValueError("Dev package must not consume or produce updater artifacts")
    permissions = overlay["app"]["security"]["capabilities"]
    if any(not isinstance(cap, dict) for cap in permissions) or "$HOME" in json.dumps(permissions):
        raise ValueError("Dev capabilities must not inherit legacy production storage access")


def linux_config(profile, base, overlay, ort_library):
    def merge(left, right):
        result = copy.deepcopy(left)
        for key, value in right.items():
            result[key] = merge(result.get(key, {}), value) if isinstance(value, dict) else copy.deepcopy(value)
        return result

    config = merge(base, overlay)
    name = profile["linux"]["packageName"]
    config["mainBinaryName"] = name
    config["bundle"]["targets"] = ["deb"]
    config["bundle"]["linux"]["deb"]["files"] = {f"/usr/lib/{name}/libonnxruntime.so": ort_library}
    # The Dev package owns its requirements, independently of production's RPM
    # policy. Use the shared-library capability rather than a distro package name.
    config["bundle"]["linux"].setdefault("rpm", {})["depends"] = copy.deepcopy(profile["linux"]["runtimeDependencies"])
    validate_config(profile, config)
    return config


def verify_linux_rpm_requirements(package, profile):
    name = subprocess.check_output(["rpm", "-qp", "--queryformat", "%{NAME}", str(package)], text=True).strip()
    if name != profile["linux"]["packageName"]:
        raise ValueError("Dev RPM has the wrong package name")
    requires = set(subprocess.check_output(["rpm", "-qp", "--requires", str(package)], text=True).splitlines())
    expected = set(profile["linux"]["runtimeDependencies"])
    if not expected.issubset(requires):
        raise ValueError("Dev RPM is missing a required runtime-library capability")
    return {"runtime_requirements": sorted(expected)}


def normalize_linux_payload(payload, profile):
    name = profile["linux"]["packageName"]
    # Tauri names the launcher after productName, not mainBinaryName, and its
    # recursive config merge retains the base application's extra file mappings.
    applications = payload / "usr/share/applications"
    original = applications / f'{profile["productName"]}.desktop'
    destination = applications / f"{name}.desktop"
    if original.exists():
        if destination.exists():
            raise ValueError("Dev staging has conflicting launchers")
        original.rename(destination)
    desktop = configparser.ConfigParser(interpolation=None)
    desktop.optionxform = str
    desktop.read(destination)
    entry = desktop["Desktop Entry"]
    if shlex.split(entry["Exec"]) not in ([name], [name, "%U"], [name, "%u"]):
        raise ValueError("Dev staging launcher has the wrong executable")
    entry["Exec"] = f"{name} %u"
    with destination.open("w") as stream:
        desktop.write(stream, space_around_delimiters=False)
    inherited = payload / "usr/lib/maple/libonnxruntime.so"
    if inherited.exists():
        inherited.unlink()
    return verify_linux_payload(payload, profile)


def verify_linux_payload(payload, profile):
    name = profile["linux"]["packageName"]
    executable = payload / "usr/bin" / name
    if not executable.is_file() or not os.access(executable, os.X_OK):
        raise ValueError("Dev executable is missing or not executable")
    allowed = {Path("usr/bin") / name, Path("usr/lib") / name / "libonnxruntime.so",
               Path("usr/share/applications") / f"{name}.desktop"}
    icons = []
    for path in payload.rglob("*"):
        if path.is_symlink():
            raise ValueError("Dev payload must not link to another installation")
        if not path.is_file() and not path.is_symlink():
            continue
        relative = path.relative_to(payload)
        if relative in allowed:
            continue
        if relative.parts[:4] == ("usr", "share", "icons", "hicolor") and path.name == f"{name}.png":
            icons.append(relative)
            continue
        raise ValueError(f"Unexpected or production-owned path in Dev payload: {relative}")
    if not icons or not (payload / "usr/lib" / name / "libonnxruntime.so").is_file():
        raise ValueError("Dev payload is missing its isolated icon or ONNX Runtime")
    desktop = configparser.ConfigParser(interpolation=None)
    desktop.read(payload / "usr/share/applications" / f"{name}.desktop")
    entry = desktop["Desktop Entry"]
    if entry["Name"] != profile["productName"] or entry["Icon"] != name:
        raise ValueError("Dev launcher has the wrong name or icon")
    if shlex.split(entry["Exec"]) not in ([name, "%U"], [name, "%u"], [f"/usr/bin/{name}", "%U"], [f"/usr/bin/{name}", "%u"]):
        raise ValueError("Dev launcher does not invoke only its own executable")
    schemes = [value for value in entry.get("MimeType", "").split(";") if value]
    if schemes != [f'x-scheme-handler/{profile["scheme"]}']:
        raise ValueError("Dev launcher must own only its Dev callback scheme")
    return {"package_name": name, "bundle_identifier": profile["identifier"], "scheme": profile["scheme"],
            "executable_sha256": hashlib.sha256(executable.read_bytes()).hexdigest()}


def verify_bundle(bundle, profile):
    with (bundle / "Contents/Info.plist").open("rb") as stream:
        info = plistlib.load(stream)
    if info["CFBundleIdentifier"] != profile["identifier"]:
        raise ValueError("Packaged bundle has the wrong identity")
    if info["CFBundleName"] != profile["productName"]:
        raise ValueError("Packaged bundle has the wrong name")
    schemes = [scheme for entry in info.get("CFBundleURLTypes", [])
               for scheme in entry.get("CFBundleURLSchemes", [])]
    if schemes != [profile["scheme"]]:
        raise ValueError("Packaged bundle has the wrong callback schemes")
    binary = bundle / "Contents/MacOS" / info["CFBundleExecutable"]
    return {"bundle_identifier": info["CFBundleIdentifier"], "scheme": schemes[0],
            "executable_sha256": hashlib.sha256(binary.read_bytes()).hexdigest()}


def verify_macos_runtime_paths(binary):
    loads = subprocess.check_output(["/usr/bin/otool", "-L", str(binary)], text=True)
    paths = re.findall(r"^\s+(.+?) \(compatibility version ", loads, re.MULTILINE)
    if not paths:
        raise ValueError("Packaged executable has no inspectable Mach-O dependencies")
    commands = subprocess.check_output(["/usr/bin/otool", "-l", str(binary)], text=True)
    is_rpath = False
    for line in commands.splitlines():
        if line.strip().startswith("cmd "):
            is_rpath = line.strip() == "cmd LC_RPATH"
        elif is_rpath:
            match = re.match(r"\s*path (.+?) \(offset \d+\)", line)
            if match:
                paths.append(match[1])
                is_rpath = False
    for path in paths:
        if path.startswith(("@rpath/", "@loader_path/", "@executable_path/")):
            continue
        if not os.path.normpath(path).startswith(("/usr/lib/", "/System/Library/")):
            raise ValueError(f"Packaged executable depends on a build-host runtime path: {path}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=["environment", "linux-config", "stage-linux", "verify", "verify-linux"])
    parser.add_argument("--bundle", type=Path)
    parser.add_argument("--rpm", type=Path)
    parser.add_argument("--ort-library")
    args = parser.parse_args()
    profile = json.loads(PROFILE.read_text())
    overlay = json.loads((TAURI / "tauri.desktop-dev.conf.json").read_text())
    validate_config(profile, overlay)
    if args.command == "environment":
        for key, value in profile["environment"].items():
            print(f"export {key}={shlex.quote(value)}")
    elif args.command == "linux-config":
        if not args.ort_library:
            parser.error("linux-config requires --ort-library")
        print(json.dumps(linux_config(profile, json.loads((TAURI / "tauri.conf.json").read_text()), overlay, args.ort_library)))
    else:
        if args.bundle is None:
            parser.error("verify requires --bundle")
        if any(os.environ.get(key) != value for key, value in profile["environment"].items()):
            raise ValueError("Build environment drifted from the fixed Dev profile")
        if args.command == "stage-linux":
            evidence = normalize_linux_payload(args.bundle, profile)
        elif args.command == "verify-linux":
            if args.rpm is None:
                parser.error("verify-linux requires --rpm")
            evidence = verify_linux_payload(args.bundle, profile)
            evidence.update(verify_linux_rpm_requirements(args.rpm, profile))
        else:
            evidence = verify_bundle(args.bundle, profile)
            verify_macos_runtime_paths(args.bundle / "Contents/MacOS/maple")
        print(json.dumps({"profile": profile, "artifact": evidence,
                          "frontend_sha256": os.environ["MAPLE_FRONTEND_DIST_TREE_SHA256"]}, indent=2))


if __name__ == "__main__":
    main()
