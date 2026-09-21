#!/usr/bin/env python3
"""Check the npm tarball using only its frozen, declared dependency closure.

The SDK directory must have been installed with its committed frozen Bun lock.
The consumer is outside the checkout and receives package copies, not symlinks
back into the build tree. No installation, registry resolution or hooks run.
"""

from __future__ import annotations

import json
from pathlib import Path, PurePosixPath
import shutil
import subprocess
import sys
import tarfile
import tempfile


def run(command: list[str], directory: Path) -> str:
    result = subprocess.run(command, cwd=directory, check=True, text=True, capture_output=True)
    return result.stdout.strip()


def resolve_package(name: str, from_path: Path, sdk: Path) -> Path | None:
    """Resolve only inside the frozen source installation, using Node's layout."""
    current = from_path
    while current == sdk or sdk in current.parents:
        candidate = current / "node_modules" / name
        if (candidate / "package.json").is_file():
            resolved = candidate.resolve()
            if not resolved.is_relative_to(sdk / "node_modules"):
                raise ValueError(f"Dependency escapes frozen installation: {name}")
            return candidate
        current = current.parent
    return None


def copy_closure(sdk: Path, consumer: Path, dependencies: dict[str, str]) -> int:
    copied: set[Path] = set()

    def copy(name: str, source_parent: Path, optional: bool = False) -> None:
        source = resolve_package(name, source_parent, sdk)
        if source is None:
            if optional:
                return
            raise ValueError(f"Missing declared dependency in frozen installation: {name}")
        if source in copied:
            return
        copied.add(source)
        destination = consumer / source.relative_to(sdk)
        # Nested dependencies are copied individually from their declarations.
        shutil.copytree(source, destination, ignore=shutil.ignore_patterns("node_modules"))
        manifest = json.loads((source / "package.json").read_text())
        optional_dependencies = manifest.get("optionalDependencies", {})
        peer_meta = manifest.get("peerDependenciesMeta", {})
        for dependency in manifest.get("dependencies", {}):
            copy(dependency, source, dependency in optional_dependencies)
        for dependency in optional_dependencies:
            copy(dependency, source, True)
        for dependency in manifest.get("peerDependencies", {}):
            copy(dependency, source, peer_meta.get(dependency, {}).get("optional", False))

    for dependency in dependencies:
        copy(dependency, sdk)
    return len(copied)


def extract_package(tarball: Path, destination: Path) -> dict:
    seen: set[str] = set()
    with tarfile.open(tarball, "r:gz") as archive:
        for member in archive.getmembers():
            path = PurePosixPath(member.name)
            if (
                not member.isfile()
                or not path.parts
                or path.parts[0] != "package"
                or len(path.parts) < 2
                or ".." in path.parts
                or path.is_absolute()
                or member.name in seen
            ):
                raise ValueError(f"Unexpected package archive member: {member.name}")
            seen.add(member.name)
            target = destination.joinpath(*path.parts[1:])
            target.parent.mkdir(parents=True, exist_ok=True)
            contents = archive.extractfile(member)
            if contents is None:
                raise ValueError(f"Missing package archive content: {member.name}")
            target.write_bytes(contents.read())
    manifest = json.loads((destination / "package.json").read_text())
    if manifest["name"] != "@mapleai/sdk":
        raise ValueError("Expected @mapleai/sdk tarball")
    return manifest


def check(sdk: Path, tarball: Path, consumer: Path) -> None:
    package_directory = consumer / "node_modules" / "@mapleai" / "sdk"
    manifest = extract_package(tarball, package_directory)
    source_manifest = json.loads((sdk / "package.json").read_text())
    for key in ("name", "version", "dependencies", "peerDependencies", "exports"):
        if manifest.get(key) != source_manifest.get(key):
            raise ValueError(f"Packaged {key} does not match the SDK manifest")

    dependencies = {**manifest.get("dependencies", {}), **manifest.get("peerDependencies", {})}
    # These are consumer tooling, not undeclared SDK runtime dependencies.
    for name in ("typescript", "@types/react"):
        dependencies[name] = source_manifest["devDependencies"][name]
    count = copy_closure(sdk, consumer, dependencies)
    (consumer / "package.json").write_text(json.dumps({"private": True, "type": "module"}))
    symbols = [
        "OpenSecretProvider", "useOpenSecret", "createCustomFetch",
        "OpenSecretDeveloper", "useOpenSecretDeveloper", "OpenSecretInferenceCapacityError",
    ]
    assertion = (
        f"for (const name of {json.dumps(symbols)}) assert.equal(typeof sdk[name], 'function', name);\n"
        "console.log(JSON.stringify(Object.keys(sdk).sort()));\n"
    )
    (consumer / "consumer.mjs").write_text(
        'import assert from "node:assert/strict";\nimport * as sdk from "@mapleai/sdk";\n' + assertion
    )
    (consumer / "consumer.cjs").write_text(
        'const assert = require("node:assert/strict");\nconst sdk = require("@mapleai/sdk");\n' + assertion
    )
    (consumer / "browser-umd.cjs").write_text(
        'const assert = require("node:assert/strict");\n'
        'const fs = require("node:fs");\nconst vm = require("node:vm");\n'
        'const { webcrypto } = require("node:crypto");\n'
        'const context = vm.createContext({ React: require("react"), console, URL, '
        'TextEncoder, TextDecoder, crypto: webcrypto });\n'
        'vm.runInContext(fs.readFileSync(require.resolve("@mapleai/sdk"), "utf8"), context);\n'
        'assert.equal(context.OpenSecretReact, undefined);\nconst sdk = context.MapleSDK;\n' + assertion
    )
    types = (
        'import { OpenSecretProvider, useOpenSecret, createCustomFetch } from "@mapleai/sdk";\n'
        'import type { Model, OpenSecretContextType, PcrEnvironment } from "@mapleai/sdk";\n'
        'const env: PcrEnvironment = "production";\n'
        'const model: Model = { id: "example", created: 0, object: "model", owned_by: "example" };\n'
        'type Context = OpenSecretContextType;\n'
        'declare const context: Context;\n'
        'for (const initiate of [context.initiateGitHubAuth, context.initiateGoogleAuth, '
        'context.initiateAppleAuth]) {\n'
        '  void initiate("");\n'
        '  void initiate("invite", "https://auth.example.com/callback");\n'
        '}\n'
        'void [OpenSecretProvider, useOpenSecret, createCustomFetch, env, model];\n'
    )
    for name in ("consumer.ts", "consumer.mts"):
        (consumer / name).write_text(types)
    (consumer / "consumer.cts").write_text(
        'import sdk = require("@mapleai/sdk");\n'
        'const env: sdk.PcrEnvironment = "production";\n'
        'const model: sdk.Model = { id: "example", created: 0, object: "model", owned_by: "example" };\n'
        'declare const context: sdk.OpenSecretContextType;\n'
        'for (const initiate of [context.initiateGitHubAuth, context.initiateGoogleAuth, '
        'context.initiateAppleAuth]) {\n'
        '  void initiate("");\n'
        '  void initiate("invite", "https://auth.example.com/callback");\n'
        '}\n'
        'void [sdk.OpenSecretProvider, sdk.useOpenSecret, env, model];\n'
    )
    runtime_exports = [json.loads(run(["node", name], consumer)) for name in (
        "consumer.mjs", "consumer.cjs", "browser-umd.cjs",
    )]
    if not all(exports == runtime_exports[0] for exports in runtime_exports):
        raise ValueError("ESM, CommonJS and browser UMD runtime exports differ")
    compiler = ["node", "node_modules/typescript/bin/tsc", "--strict", "--skipLibCheck", "false",
                "--target", "ES2020", "--noEmit"]
    run([*compiler, "--module", "ESNext", "--moduleResolution", "Bundler", "consumer.ts"], consumer)
    run([*compiler, "--module", "NodeNext", "--moduleResolution", "NodeNext",
         "consumer.mts", "consumer.cts"], consumer)
    print(f"Package consumers passed: ESM, CommonJS, browser UMD, Bundler and NodeNext; "
          f"{count} frozen dependency/tool packages, {len(runtime_exports[0])} runtime exports.")


def main() -> None:
    if len(sys.argv) != 3:
        raise SystemExit("Usage: check-sdk-package-consumers.py SDK_DIR TARBALL")
    sdk, tarball = (Path(argument).resolve() for argument in sys.argv[1:])
    with tempfile.TemporaryDirectory(prefix="maple-sdk-consumer-") as directory:
        check(sdk, tarball, Path(directory))


if __name__ == "__main__":
    try:
        main()
    except subprocess.CalledProcessError as error:
        print(error.stdout or "", file=sys.stderr)
        print(error.stderr or "", file=sys.stderr)
        raise SystemExit(error.returncode) from error
