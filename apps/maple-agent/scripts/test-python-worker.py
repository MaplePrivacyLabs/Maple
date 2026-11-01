#!/usr/bin/env python3
"""Run focused worker tests with the explicitly prepared package interpreter.

This command only consumes resources. Run just python-prepare before invoking it.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess


REPO = Path(__file__).resolve().parent.parent


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path, default=Path(os.environ.get(
        "MAPLE_CODE_MODE_RUNTIME_MANIFEST", REPO / "target/debug/runtime/python/runtime.json"
    )))
    args = parser.parse_args()
    try:
        manifest_path = args.manifest.resolve(strict=True)
        manifest = json.loads(manifest_path.read_text())
        if manifest["implementation"] != "cpython" or manifest["version"] != "3.13.15":
            raise ValueError("Tests require the declared CPython 3.13.15 fixture")
        interpreter = (manifest_path.parent / manifest["executable"]).resolve(strict=True)
        worker = (manifest_path.parent / manifest["worker"]).resolve(strict=True)
        source = REPO / "crates/maple-code-mode/python/worker.py"
        if hashlib.sha256(worker.read_bytes()).digest() != hashlib.sha256(source.read_bytes()).digest():
            raise ValueError("Prepared worker differs from source; run just python-prepare")
        subprocess.run([
            str(interpreter), "-I", "-B", "-u",
            str(source.with_name("test_worker.py")), "--python", str(interpreter),
        ], check=True)
    except (OSError, ValueError, KeyError) as error:
        parser.exit(1, f"Python worker tests require valid prepared resources: {error}. Run just python-prepare.\n")


if __name__ == "__main__":
    main()
