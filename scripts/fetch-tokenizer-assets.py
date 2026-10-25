#!/usr/bin/env python3
"""Fetch the reviewed tokenizer assets for a non-Nix build, never at runtime.

The backend Nix shell uses fixed-output derivations from the same manifest. This
helper supports explicit non-Nix builds. It prints only the verified
absolute directory, suitable for MAPLE_TOKENIZER_ASSETS_DIR at compile time.
"""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import re
import sys
import tempfile
from urllib.parse import urlsplit
from urllib.request import Request, urlopen


MANIFEST = Path(__file__).resolve().parents[1] / "crates/model-tokenizer/assets/manifest.json"
MAX_ASSET_BYTES = 64 * 1024 * 1024
CHUNK_BYTES = 1024 * 1024


def load_manifest(path: Path) -> list[dict]:
    manifest = json.loads(path.read_text(encoding="utf-8"))
    if manifest.get("schema_version") != 1 or not isinstance(manifest.get("assets"), list):
        raise ValueError("unsupported tokenizer asset manifest")
    assets = manifest["assets"]
    if not assets:
        raise ValueError("empty tokenizer asset manifest")
    seen_ids: set[str] = set()
    seen_files: set[str] = set()
    for asset in assets:
        digest = asset.get("sha256", "")
        asset_id = asset.get("id", "")
        extension = {"huggingface_json": ".json", "tiktoken_ranks": ".tiktoken"}.get(asset.get("format"))
        if not re.fullmatch(r"[a-z][a-z0-9_]*", asset_id) or asset_id in seen_ids:
            raise ValueError("invalid or duplicate tokenizer asset id")
        if not re.fullmatch(r"[0-9a-f]{64}", digest) or not extension:
            raise ValueError(f"invalid hash or format for {asset_id}")
        if asset.get("filename") != digest + extension or asset["filename"] in seen_files:
            raise ValueError(f"invalid or duplicate filename for {asset_id}")
        size = asset.get("size_bytes")
        if type(size) is not int or not 0 < size <= MAX_ASSET_BYTES:
            raise ValueError(f"invalid asset size for {asset_id}")
        sources = asset.get("sources")
        if not isinstance(sources, list) or not sources:
            raise ValueError(f"missing sources for {asset_id}")
        for source in sources:
            revision = source.get("revision", "")
            repo = source.get("repository", "")
            if not re.fullmatch(r"[0-9a-f]{40}", revision) or not re.fullmatch(r"[\w.-]+/[\w.-]+", repo):
                raise ValueError(f"unpinned source for {asset_id}")
            url = urlsplit(source.get("url", ""))
            prefix = f"/{repo}/resolve/{revision}/"
            if (url.scheme != "https" or url.netloc != "huggingface.co" or url.query or url.fragment
                    or not url.path.startswith(prefix) or "/" in url.path.removeprefix(prefix)
                    or url.path.removeprefix(prefix) not in {"tokenizer.json", "tiktoken.model"}):
                raise ValueError(f"invalid pinned source URL for {asset_id}")
        seen_ids.add(asset_id)
        seen_files.add(asset["filename"])
    return assets


def verify(path: Path, asset: dict) -> None:
    if path.stat().st_size != asset["size_bytes"]:
        raise ValueError(f"tokenizer asset size mismatch: {asset['id']}")
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(CHUNK_BYTES), b""):
            digest.update(chunk)
    if digest.hexdigest() != asset["sha256"]:
        raise ValueError(f"tokenizer asset hash mismatch: {asset['id']}")


def download(asset: dict, output: Path) -> None:
    # Mirrors are permitted only for byte-identical content with the same hash.
    for source in asset["sources"]:
        temp_path = None
        try:
            request = Request(source["url"], headers={"User-Agent": "maple-tokenizer-build/1"})
            with urlopen(request, timeout=60) as response, tempfile.NamedTemporaryFile(
                dir=output, prefix=".tokenizer-", delete=False
            ) as temp:
                temp_path = Path(temp.name)
                received = 0
                while chunk := response.read(CHUNK_BYTES):
                    received += len(chunk)
                    if received > asset["size_bytes"]:
                        raise ValueError(f"tokenizer asset exceeds pinned size: {asset['id']}")
                    temp.write(chunk)
            verify(temp_path, asset)
            temp_path.replace(output / asset["filename"])
            return
        except (OSError, ValueError):
            # Avoid logging remote bodies or incidental redirect URLs.
            pass
        finally:
            if temp_path is not None:
                temp_path.unlink(missing_ok=True)
    raise ValueError(f"could not fetch a hash-verified tokenizer asset: {asset['id']}")


def prepare(manifest: Path, output: Path, check_only: bool = False) -> Path:
    assets = load_manifest(manifest)
    output = output.expanduser().resolve()
    if not check_only:
        output.mkdir(parents=True, exist_ok=True)
    for asset in assets:
        path = output / asset["filename"]
        if path.exists():
            # Never silently replace a corrupt or unexpected cached artifact.
            verify(path, asset)
        elif check_only:
            raise ValueError(f"missing tokenizer asset: {asset['id']}")
        else:
            download(asset, output)
    return output


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path, default=MANIFEST)
    parser.add_argument("--output", type=Path, required=True, help="explicit build-time asset directory")
    parser.add_argument("--check", action="store_true", help="verify a complete directory without network or writes")
    args = parser.parse_args()
    try:
        output = prepare(args.manifest, args.output, args.check)
    except (OSError, ValueError, KeyError, TypeError) as error:
        print(f"tokenizer assets: {error}", file=sys.stderr)
        return 1
    print(output)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
