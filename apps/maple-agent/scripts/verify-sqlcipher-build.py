#!/usr/bin/env python3
"""Check the selected SQLCipher source route and supported-target crypto graphs."""

import argparse
import json
from pathlib import Path
import subprocess
import sys


COMPONENT = Path(__file__).resolve().parents[1]
TARGETS = (
    "aarch64-apple-darwin",
    "x86_64-unknown-linux-gnu",
    "x86_64-pc-windows-msvc",
    "aarch64-pc-windows-msvc",
)


def verify(metadata_path):
    metadata = json.loads(metadata_path.read_text(encoding="utf-8"))
    packages = [package for package in metadata["packages"] if package["name"] == "rusqlite"]
    if (
        len(packages) != 1 or packages[0]["version"] != "0.40.2"
        or packages[0]["source"] != "registry+https://github.com/rust-lang/crates.io-index"
    ):
        raise ValueError("expected the released rusqlite 0.40.2 registry source")
    subprocess.run(
        [sys.executable, str(COMPONENT / "dependencies/sqlcipher/verify_sources.py"),
         "--metadata", str(metadata_path)],
        check=True,
    )
    for target in TARGETS:
        result = subprocess.run(
            ["cargo", "tree", "--locked", "--workspace", "--all-features", "--target", target,
             "-i", "openssl-sys"],
            cwd=COMPONENT, text=True, encoding="utf-8", capture_output=True, check=False,
        )
        if result.returncode != 0:
            raise ValueError(f"crypto graph check failed for {target}: {result.stderr[-2000:]}")
        if result.stdout.strip():
            raise ValueError(f"an additional openssl-sys provider is active for {target}")
    print("SQLCipher pins and supported-target crypto graphs match the reviewed source route.")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("metadata", type=Path)
    arguments = parser.parse_args()
    verify(arguments.metadata.resolve())


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, KeyError, subprocess.CalledProcessError) as error:
        print(f"SQLCipher build verification failed: {error}", file=sys.stderr)
        sys.exit(1)
