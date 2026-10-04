#!/usr/bin/env python3
"""Check external wheel installation in a copied PBS runtime, entirely offline.

Pass --runtime target/debug/runtime/python or --app /path/to/a/signed/Maple.app.
The app variant also verifies the copied macOS signature before and after use.
This consumes existing artifacts; it does not build or prepare Python.
"""

import argparse
import base64
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import zipfile


REPO = Path(__file__).resolve().parent.parent


def snapshot(root):
    result = {}
    for path in sorted(root.rglob("*")):
        metadata = path.lstat()
        if path.is_symlink():
            content = ("link", os.readlink(path))
        elif path.is_file():
            with path.open("rb") as stream:
                content = ("file", hashlib.file_digest(stream, "sha256").hexdigest())
        else:
            content = ("directory",)
        result[str(path.relative_to(root))] = (metadata.st_mode, content)
    return result


def make_wheel(directory):
    name = "maple_dependency_fixture-1.0.dist-info"
    files = {
        "maple_dependency_fixture.py": b"def double(value):\n    return value * 2\n",
        f"{name}/METADATA": (
            b"Metadata-Version: 2.1\nName: maple-dependency-fixture\nVersion: 1.0\n"
        ),
        f"{name}/WHEEL": (
            b"Wheel-Version: 1.0\nGenerator: maple-test\nRoot-Is-Purelib: true\n"
            b"Tag: py3-none-any\n"
        ),
    }
    records = []
    for path, content in files.items():
        digest = base64.urlsafe_b64encode(hashlib.sha256(content).digest()).rstrip(b"=")
        records.append(f"{path},sha256={digest.decode()},{len(content)}\n")
    files[f"{name}/RECORD"] = ("".join(records) + f"{name}/RECORD,,\n").encode()
    wheel = directory / "maple_dependency_fixture-1.0-py3-none-any.whl"
    with zipfile.ZipFile(wheel, "w") as archive:
        for path, content in files.items():
            archive.writestr(path, content)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument("--runtime", type=Path)
    source.add_argument("--app", type=Path)
    args = parser.parse_args()
    if args.app and sys.platform != "darwin":
        parser.error("--app requires macOS codesign")
    with tempfile.TemporaryDirectory(prefix="maple dependencies 日本語 ") as temporary:
        root = Path(temporary)
        copied = root / ("Maple Dependency Check.app" if args.app else "python")
        shutil.copytree((args.app or args.runtime).resolve(strict=True), copied, symlinks=True)
        runtime = copied / "Contents/Resources/python" if args.app else copied
        manifest = json.loads((runtime / "runtime.json").read_text())
        if not manifest["distribution"].startswith("pbs-"):
            raise ValueError("This pip check requires the portable PBS runtime")
        if args.app:
            subprocess.run(["/usr/bin/codesign", "--verify", "--deep", "--strict", str(copied)], check=True)
        before = snapshot(copied)
        fixture = root / "wheels"
        fixture.mkdir()
        make_wheel(fixture)
        task = root / "task"
        task.mkdir()
        deps = task / "external dependencies"
        spec = importlib.util.spec_from_file_location(
            "maple_dependency_worker_tests", REPO / "crates/maple-code-mode/python/test_worker.py"
        )
        harness = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(harness)
        harness.PYTHON = (runtime / manifest["executable"]).resolve(strict=True)
        harness.WORKER = (runtime / manifest["worker"]).resolve(strict=True)
        worker = harness.NativeWorker(task)

        def execute(code, expected):
            messages = worker.execute(code)
            if messages[-1].get("status") != "ok" or harness.result(messages) != expected:
                raise AssertionError(messages)

        try:
            execute(f"""
import importlib, pathlib, subprocess, sys
deps = pathlib.Path({str(deps)!r})
installed = subprocess.run([
    sys.executable, '-I', '-B', '-m', 'pip', '--isolated',
    '--disable-pip-version-check', 'install', '--no-cache-dir',
    '--only-binary=:all:', '--target', str(deps),
    '--no-index', '--find-links', {str(fixture)!r}, 'maple-dependency-fixture==1.0',
], capture_output=True, text=True, timeout=20)
assert installed.returncode == 0, (installed.stdout, installed.stderr)
sys.path.insert(0, str(deps))
importlib.invalidate_caches()
import maple_dependency_fixture
assert pathlib.Path(maple_dependency_fixture.__file__).parent == deps
value = maple_dependency_fixture.double(21)
value
""", "42")
            execute("value = maple_dependency_fixture.double(value)\nvalue", "84")
            execute("""
import multiprocessing
from concurrent.futures import ProcessPoolExecutor
spawn = multiprocessing.get_context('spawn')
with spawn.Pool(1) as pool:
    pooled = pool.map_async(maple_dependency_fixture.double, [2, 3]).get(timeout=3)
with ProcessPoolExecutor(max_workers=1, mp_context=spawn) as executor:
    executed = executor.submit(maple_dependency_fixture.double, 4).result(timeout=3)
(pooled, executed)
""", "([4, 6], 8)")
        finally:
            worker.shutdown()
        after = snapshot(copied)
        changed = [path for path in before.keys() | after.keys() if before.get(path) != after.get(path)]
        if changed:
            raise AssertionError(f"Packaged tree changed: {sorted(changed)}")
        if args.app:
            subprocess.run(["/usr/bin/codesign", "--verify", "--deep", "--strict", str(copied)], check=True)
        print("Offline external wheel install, retained import, and imported-module spawn pools passed")
        print(f"Packaged tree unchanged: {len(before)} entries; signature checked: {bool(args.app)}")


if __name__ == "__main__":
    main()
