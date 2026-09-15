#!/usr/bin/env python3
"""Archive a built Maple executable together with its prepared PBS runtime."""

import argparse
import hashlib
import json
from pathlib import Path
import shutil
import tempfile


def package(binary, runtime, name, output):
    if Path(name).name != name or name in ("", ".", ".."):
        raise ValueError("Archive name must be one filename component")
    manifest = json.loads((runtime / "runtime.json").read_text())
    if not manifest["distribution"].startswith("pbs-"):
        raise ValueError("Portable archives require the pinned PBS runtime; Nix has its own closure")
    if not binary.is_file() or not (runtime / manifest["executable"]).is_file() or not (runtime / manifest["worker"]).is_file():
        raise ValueError("Archive requires the built executable and complete prepared Python runtime")
    output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=".maple-archive-", dir=output) as temporary:
        staging = Path(temporary) / name
        staging.mkdir()
        shutil.copy2(binary, staging / binary.name)
        shutil.copytree(runtime, staging / "runtime/python", symlinks=True)
        shutil.copy2(Path(__file__).resolve().parent.parent / "LICENSE", staging / "LICENSE")
        archive_format = "zip" if binary.suffix.lower() == ".exe" else "gztar"
        archive = Path(shutil.make_archive(str(Path(temporary) / name), archive_format, temporary, name))
        destination = output / archive.name
        archive.replace(destination)
        with destination.open("rb") as source:
            checksum = hashlib.file_digest(source, "sha256").hexdigest()
        destination.with_name(destination.name + ".sha256").write_text(f"{checksum}  {destination.name}\n")
    print(destination)
    return destination


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--runtime", type=Path, required=True)
    parser.add_argument("--name", required=True)
    parser.add_argument("--output-dir", type=Path, default=Path("dist"))
    args = parser.parse_args()
    package(args.binary.resolve(), args.runtime.resolve(), args.name, args.output_dir.resolve())


if __name__ == "__main__":
    main()
