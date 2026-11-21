#!/usr/bin/env python3
"""Check that the packed SDK ships its release bundles exactly as published.

Each bundle under sdk/src/lib/releaseEvidence/published/ has its SHA-256 in a
.sha256 file beside it. This check confirms each file still matches its pin,
then finds the bundle's text in the packed ESM build and in the packed UMD
build and requires the same hash there, so the package verifies the published
bytes rather than a reformatted copy. It also confirms that no WebAssembly
ships. check-sdk-package-consumers.py loads both builds; tests in the SDK
verify the published bytes.

Usage: check-sdk-packed-release-evidence.py <sdk-dir> <package.tgz>
"""

from __future__ import annotations

import ast
import hashlib
from pathlib import Path
import re
import sys
import tarfile

PUBLISHED = Path("src/lib/releaseEvidence/published")
UMD_BUILD = "package/dist/maple-sdk.umd.cjs"
ESM_BUILD = re.compile(r"package/dist/[^/]+\.js")
# A JavaScript string literal that starts a Sigstore bundle. The bundler
# emits the bundle text as one quoted literal; within it, a quote of the same
# kind only appears escaped.
BUNDLE_LITERAL = re.compile(r"""(['"])\{(?:(?!\1)[^\\\n]|\\.)*\1""")
BUNDLE_START = '{"mediaType":"application/vnd.dev.sigstore.bundle'


def sha256_hex(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def pinned_hashes(sdk_dir: Path) -> dict[str, str]:
    """The pinned hash of each published bundle, after checking the file."""
    pins = {}
    for bundle in sorted((sdk_dir / PUBLISHED).glob("*.sigstore.json")):
        pin_file = bundle.with_name(bundle.name + ".sha256")
        pinned = pin_file.read_text().split()[0]
        actual = sha256_hex(bundle.read_bytes())
        matches_pin = actual == pinned
        if not matches_pin:
            raise SystemExit(f"{bundle.name} does not match its pinned SHA-256")
        pins[bundle.name] = pinned
    if not pins:
        raise SystemExit("No published release bundles found")
    return pins


def embedded_bundle_hashes(source: str) -> set[str]:
    """Hashes of the bundle texts embedded as string literals in one build."""
    hashes = set()
    for match in BUNDLE_LITERAL.finditer(source):
        literal = match.group(0)
        names_bundle_media_type = "sigstore.bundle" in literal
        if not names_bundle_media_type:
            continue
        # Python reads the escapes these literals use (\\, \', \", \n, \u)
        # the same way JavaScript does; literal_eval runs no code.
        text = ast.literal_eval(literal)
        is_bundle = text.startswith(BUNDLE_START)
        if is_bundle:
            hashes.add(sha256_hex(text.encode("utf-8")))
    return hashes


def read_builds(tarball: Path) -> tuple[str, str]:
    """The UMD build, and every ESM file joined, as text."""
    umd = ""
    esm_parts = []
    with tarfile.open(tarball) as archive:
        for member in archive.getmembers():
            ships_wasm = member.name.endswith(".wasm")
            if ships_wasm:
                raise SystemExit(f"The package ships WebAssembly: {member.name}")
            is_umd = member.name == UMD_BUILD
            is_esm = ESM_BUILD.fullmatch(member.name) is not None
            if not (is_umd or is_esm):
                continue
            text = archive.extractfile(member).read().decode("utf-8")
            if is_umd:
                umd = text
            else:
                esm_parts.append(text)
    return umd, "\n".join(esm_parts)


def main() -> None:
    expected_arguments = 3
    arguments_missing = len(sys.argv) != expected_arguments
    if arguments_missing:
        raise SystemExit("usage: check-sdk-packed-release-evidence.py <sdk-dir> <package.tgz>")
    sdk_dir = Path(sys.argv[1]).resolve()
    tarball = Path(sys.argv[2]).resolve()
    pins = pinned_hashes(sdk_dir)
    umd, esm = read_builds(tarball)
    for build_name, source in (("ESM", esm), ("UMD", umd)):
        uses_webassembly = "WebAssembly" in source
        if uses_webassembly:
            raise SystemExit(f"The {build_name} build references WebAssembly")
        embedded = embedded_bundle_hashes(source)
        for bundle_name, pinned in pins.items():
            shipped = pinned in embedded
            if not shipped:
                raise SystemExit(f"The {build_name} build does not carry {bundle_name} as published")
    print(f"Packed release evidence passed: {len(pins)} published bundle(s) in ESM and UMD, no WebAssembly.")


if __name__ == "__main__":
    main()
