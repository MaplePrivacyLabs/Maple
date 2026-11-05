#!/usr/bin/env python3
"""Select and smoke the actual MSVC linker used by Windows Agent Cargo jobs."""

from __future__ import annotations

import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys
import tempfile


HOST = "x86_64-pc-windows-msvc"
LINKER_ENV = "CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER"
MARKER = "MAPLE_WINDOWS_LINKER_SMOKE"


def target_identity(architecture: str) -> tuple[str, str]:
    if architecture not in {"x86_64", "aarch64"}:
        raise ValueError("unsupported native Windows build architecture")
    host = architecture + "-pc-windows-msvc"
    return host, "CARGO_TARGET_" + host.upper().replace("-", "_") + "_LINKER"


def required_executable(name: str) -> Path:
    selected = shutil.which(name)
    if not selected:
        raise ValueError(f"required Windows build tool is missing: {name}")
    return Path(selected).resolve()


def selected_linker(environment: dict[str, str]) -> Path:
    # A copied Windows environment loses os.environ's case-insensitive lookup.
    normalized = {key.upper(): value for key, value in environment.items()}
    if not normalized.get("VCTOOLSINSTALLDIR"):
        raise ValueError("select the MSVC developer environment before selecting the linker")
    toolset = Path(normalized["VCTOOLSINSTALLDIR"]).resolve()
    compiler = required_executable("cl.exe")
    if not compiler.is_relative_to(toolset):
        raise ValueError("cl.exe is outside the selected MSVC toolset")
    linker = (compiler.parent / "link.exe").resolve()
    if not linker.is_relative_to(toolset) or not linker.is_file():
        raise ValueError("MSVC link.exe is missing from the selected compiler directory")
    with linker.open("rb") as selected:
        if not selected.read(1):
            raise ValueError("selected MSVC link.exe is empty")
    if "\n" in str(linker) or "\r" in str(linker):
        raise ValueError("selected MSVC linker path cannot be exported")
    return linker


def smoke(cargo: Path, environment: dict[str, str]) -> None:
    # Keep Cargo's real invocation directory, flags and configuration. Only the
    # fixture's source and both output directories are disposable and separate
    # from the workspace/provider caches that this check precedes.
    with tempfile.TemporaryDirectory(prefix="maple-windows-linker-") as temporary:
        fixture = Path(temporary)
        (fixture / "src").mkdir()
        (fixture / "Cargo.toml").write_text(
            '[package]\nname = "maple-windows-linker-smoke"\nversion = "0.0.0"\n'
            'edition = "2024"\nbuild = "build.rs"\n\n[workspace]\nresolver = "2"\n',
            encoding="utf-8",
        )
        (fixture / "build.rs").write_text(
            f'fn main() {{ println!("cargo:rustc-env={MARKER}=ok"); }}\n', encoding="utf-8",
        )
        (fixture / "src/main.rs").write_text(
            f'fn main() {{ assert_eq!(env!("{MARKER}"), "ok"); }}\n', encoding="utf-8",
        )
        selected = environment.copy()
        selected["CARGO_TARGET_DIR"] = str(fixture / "target")
        selected["CARGO_BUILD_BUILD_DIR"] = str(fixture / "build")
        subprocess.run(
            [str(cargo), "build", "--offline", "--verbose", "--manifest-path", str(fixture / "Cargo.toml")],
            env=selected, check=True, timeout=120,
        )
        executable = fixture / "target/debug/maple-windows-linker-smoke.exe"
        if not executable.is_file():
            raise ValueError("Cargo did not produce the expected native Windows linker smoke executable")
        subprocess.run([str(executable)], env=selected, check=True, timeout=30)


def main() -> None:
    if len(sys.argv) != 1:
        raise ValueError("agent_windows.py takes no arguments")
    if platform.system() != "Windows":
        raise ValueError("the Agent MSVC linker check requires Windows")
    github_env = os.environ.get("GITHUB_ENV")
    if not github_env:
        raise ValueError("GITHUB_ENV is required to preserve the verified linker for later steps")
    rustc = required_executable(os.environ.get("RUSTC", os.environ.get("CARGO_BUILD_RUSTC", "rustc")))
    identity = subprocess.run(
        [str(rustc), "-vV"], capture_output=True, text=True, encoding="utf-8", errors="replace",
        check=True, timeout=30,
    ).stdout.strip()
    hosts = [line.removeprefix("host:").strip() for line in identity.splitlines() if line.startswith("host:")]
    host, linker_env = target_identity(os.environ.get("MAPLE_WINDOWS_ARCH", "x86_64"))
    if hosts != [host]:
        raise ValueError(f"Agent Windows linker check requires Rust host {host}")
    linker = selected_linker(dict(os.environ))
    print(identity, flush=True)
    print(f"Selected MSVC Cargo linker: {linker}", flush=True)
    os.environ[linker_env] = str(linker)
    smoke(required_executable("cargo"), dict(os.environ))
    # GITHUB_ENV affects subsequent steps only. Publish the selection after the
    # Python -> Cargo -> rustc build-script and executable link has succeeded.
    with Path(github_env).open("a", encoding="utf-8") as output:
        output.write(f"{linker_env}={linker}\n")
    print("Verified native Cargo build-script linking and executable startup", flush=True)


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        print(f"Agent Windows linker check failed: {error}", file=sys.stderr)
        sys.exit(1)
