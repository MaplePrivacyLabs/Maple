#!/usr/bin/env python3
"""Audit each supported platform separately, without merging incompatible TLS edges."""
import subprocess

for target in ("aarch64-apple-darwin", "x86_64-unknown-linux-gnu", "x86_64-pc-windows-msvc", "aarch64-pc-windows-msvc"):
    subprocess.run(["cargo", "deny", "--locked", "--target", target, "check", "advisories", "bans"], check=True)
