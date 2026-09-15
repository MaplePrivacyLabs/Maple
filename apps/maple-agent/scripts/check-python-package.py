#!/usr/bin/env python3
"""Exercise a copied PBS runtime offline, away from its original path and CWD."""

import argparse
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
import tarfile
import zipfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--smoke", type=Path, required=True)
    parser.add_argument("--runtime", type=Path, default=Path("target/debug/runtime/python"))
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="maple package ") as temporary:
        root = Path(temporary)
        # Exercise the same archive writer used by release CI using debug code.
        spec = importlib.util.spec_from_file_location("maple_archive", Path(__file__).with_name("package-archive.py"))
        packager = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(packager)
        archive = packager.package(args.smoke.resolve(), args.runtime.resolve(), "Relocated Maple 日本語", root / "archives")
        if archive.suffix == ".zip":
            with zipfile.ZipFile(archive) as source:
                source.extractall(root)
        else:
            with tarfile.open(archive) as source:
                source.extractall(root, filter="data")
        bundle = root / "Relocated Maple 日本語"
        runtime = bundle / "runtime/python"
        smoke = bundle / args.smoke.name
        cwd = root / "arbitrary task directory"
        cwd.mkdir()
        manifest = json.loads((runtime / "runtime.json").read_text())
        if not manifest["distribution"].startswith("pbs-"):
            raise ValueError("Relocation check requires the portable PBS fixture")
        environment = os.environ.copy()
        environment.pop("MAPLE_CODE_MODE_RUNTIME_MANIFEST", None)
        environment["PATH"] = ""
        environment["PYTHONHOME"] = "invalid ignored PYTHONHOME"
        environment["PYTHONPATH"] = "invalid ignored PYTHONPATH"
        subprocess.run([
            str(runtime / manifest["executable"]), "-I", "-B", "-u", "-c",
            'import sys, sysconfig, ssl, sqlite3, ctypes, zlib, bz2, lzma; '
            'assert sys.version_info[:3] == (3, 13, 15); '
            'assert not sysconfig.get_config_var("Py_GIL_DISABLED"); '
            'assert "" not in sys.path',
        ], cwd=cwd, env=environment, check=True, timeout=30)
        subprocess.run([str(smoke)], cwd=cwd, env=environment, check=True, timeout=60)
        size = sum(path.stat().st_size for path in runtime.rglob("*") if path.is_file() and not path.is_symlink())
        print(f"Relocated offline PBS archive passed: {size:,} installed bytes")


if __name__ == "__main__":
    main()
