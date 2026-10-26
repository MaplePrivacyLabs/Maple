#!/usr/bin/env python3
"""Scope Agent manifest cache inputs without changing the legacy fingerprint.

Only dependencies proven inactive on this native target can be omitted. The
original identity remains available for an exact, independently checked cache
migration; compiler, native ABI, flags, profiles and Cargo configuration retain
their existing guards.
"""

from __future__ import annotations

import argparse
import copy
import json
import os
from pathlib import Path, PurePosixPath, PureWindowsPath
import re
import subprocess
import sys
import tomllib

import agent_cache


# Bump this policy version when compatibility semantics change. Documentation
# or other source-only edits must not create a new cache namespace; normalized
# compiler/native/build inputs remain part of the canonical identity itself.
SCOPE_SCHEMA = 1
DEPENDENCY_KINDS = ("dependencies", "dev-dependencies", "build-dependencies")
TOKEN = re.compile(r'\s*(?:(?P<word>[A-Za-z_][A-Za-z_0-9]*)|(?P<string>"(?:[^"\\]|\\.)*")|(?P<punct>[(),=]))')
# These are the native targets selected by the Agent workflows. A triple's
# architecture is not generally its first segment (i686 uses cfg arch=x86),
# and Python's platform name cannot identify Android or another custom target.
# Unknown native targets keep every manifest input until explicitly supported.
NATIVE_HOSTS = {
    "x86_64-pc-windows-msvc": ("Windows", "windows", "x86_64"),
    "x86_64-unknown-linux-gnu": ("Linux", "linux", "x86_64"),
    "aarch64-apple-darwin": ("Darwin", "macos", "aarch64"),
    "x86_64-apple-darwin": ("Darwin", "macos", "x86_64"),
}


class CfgParser:
    """Evaluate a small known cfg subset, preserving unknowns in three values."""

    def __init__(self, expression: str, values: dict[str, str | bool]):
        self.tokens = []
        offset = 0
        while expression[offset:].strip():
            match = TOKEN.match(expression, offset)
            if match is None:
                raise ValueError("unknown cfg syntax")
            self.tokens.append((match.lastgroup, match.group(match.lastgroup)))
            offset = match.end()
        self.index = 0
        self.values = values

    def pop(self, expected: str | None = None) -> tuple[str, str]:
        if self.index >= len(self.tokens):
            raise ValueError("incomplete cfg expression")
        token = self.tokens[self.index]
        if expected is not None and token[1] != expected:
            raise ValueError("unexpected cfg token")
        self.index += 1
        return token

    def peek(self) -> str | None:
        return self.tokens[self.index][1] if self.index < len(self.tokens) else None

    def expression(self) -> bool | None:
        kind, name = self.pop()
        if kind != "word":
            raise ValueError("expected cfg identifier")
        if self.peek() == "=":
            self.pop("=")
            kind, literal = self.pop()
            if kind != "string":
                raise ValueError("expected cfg string")
            value = self.values.get(name)
            return value == json.loads(literal) if isinstance(value, str) else None
        if self.peek() != "(":
            value = self.values.get(name)
            return value if isinstance(value, bool) else None
        self.pop("(")
        children = []
        while self.peek() != ")":
            children.append(self.expression())
            if self.peek() != ",":
                break
            self.pop(",")
        self.pop(")")
        if name == "all":
            return False if False in children else (True if all(x is True for x in children) else None)
        if name == "any":
            return True if True in children else (False if all(x is False for x in children) else None)
        if name == "not" and len(children) == 1:
            return None if children[0] is None else not children[0]
        return None


def native_target(identity: dict) -> tuple[str, dict[str, str | bool]] | None:
    """Do not infer cfg when Cargo configuration or overrides can select it."""
    configuration = identity.get("configuration", {})
    if any(configuration.get("cargo_configs", {}).values()):
        return None
    environment = identity.get("environment", {})
    if any(environment.get(key) for key in (
        "CARGO_BUILD_TARGET", "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER",
        "CARGO_BUILD_RUSTC_WRAPPER", "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER",
    )):
        return None
    for key, value in environment.items():
        if "RUSTFLAGS" in key and re.search(r"--(?:cfg|target)(?:[=\s\x1f]|$)", value):
            return None
    match = re.search(r"^host:\s*(\S+)\s*$", identity.get("rustc", {}).get("version", ""), re.MULTILINE)
    if match is None:
        return None
    host = match.group(1)
    native = NATIVE_HOSTS.get(host)
    if native is None or native[0] != identity.get("platform"):
        return None
    _, target_os, architecture = native
    return host, {
        "target_os": target_os,
        "target_arch": architecture,
        "target_family": "windows" if target_os == "windows" else "unix",
        "windows": target_os == "windows",
        "unix": target_os != "windows",
    }


def target_matches(target: str, context: tuple[str, dict[str, str | bool]]) -> bool | None:
    host, values = context
    if not target.startswith("cfg("):
        # Literal target triples are Cargo's other supported target-table form.
        return target == host if re.fullmatch(r"[A-Za-z0-9_.-]+", target) else None
    if not target.endswith(")"):
        return None
    try:
        parser = CfgParser(target[4:-1], values)
        result = parser.expression()
        return result if parser.index == len(parser.tokens) else None
    except ValueError:
        return None


def manifest_path(root: Path, relative: str) -> Path:
    normalized = PurePosixPath(relative.replace("\\", "/"))
    if normalized.is_absolute() or PureWindowsPath(relative).drive or ".." in normalized.parts:
        raise ValueError("manifest identity must use a repository-relative path")
    return root.joinpath(*normalized.parts)


def scoped_manifest(settings: dict, manifest: dict, context: tuple[str, dict[str, str | bool]]) -> dict:
    result = copy.deepcopy(settings)
    declarations: dict[str, list[tuple[bool | None, object]]] = {}
    inactive = []
    for kind in DEPENDENCY_KINDS:
        for name, selection in manifest.get(kind, {}).items():
            declarations.setdefault(name, []).append((True, selection))
    for target, tables in manifest.get("target", {}).items():
        matches = target_matches(target, context)
        for kind in DEPENDENCY_KINDS:
            for name, selection in tables.get(kind, {}).items():
                declarations.setdefault(name, []).append((matches, selection))
                if matches is False:
                    inactive.append((f"target.{target}.{kind}.{name}", name,
                                     isinstance(selection, dict) and selection.get("optional") is True))

    features = manifest.get("features", {})
    # A plain optional-dependency feature or a strong dep/feature edge may
    # activate the package's own cfg(feature). Keep these cases conservative.
    ambiguous = set(features)
    explicit_dependencies = set()
    for selections in features.values():
        for selection in selections:
            if selection.startswith("dep:"):
                explicit_dependencies.add(selection[4:])
            if not selection.startswith("dep:") and "?/" not in selection:
                ambiguous.add(selection.split("/", 1)[0])
    inactive_optional = {
        name for name, selections in declarations.items()
        if name not in ambiguous and name in explicit_dependencies and all(
            matches is False and isinstance(selection, dict) and selection.get("optional") is True
            for matches, selection in selections
        )
    }
    dependency_features = result.get("dependency-features", {})
    for key, name, optional in inactive:
        # Even an inactive optional declaration can create a local implicit
        # feature. Do not erase its input when feature edges are ambiguous.
        # Without dep:name Cargo creates an implicit local feature, including
        # under --all-features, even if the target dependency is not compiled.
        if name not in ambiguous and (not optional or name in inactive_optional):
            dependency_features.pop(key, None)
    for name, selections in result.get("features", {}).items():
        result["features"][name] = [
            selection for selection in selections
            if not (
                selection.startswith("dep:") and selection[4:] in inactive_optional
                or "?/" in selection and selection.split("?/", 1)[0] in inactive_optional
            )
        ]
    return result


def canonical_identity(identity: dict, root: Path = agent_cache.ROOT) -> dict:
    result = copy.deepcopy(identity)
    result["scope_schema"] = SCOPE_SCHEMA
    context = native_target(identity)
    if context is None:
        return result
    manifests = result.get("configuration", {}).get("manifests", {})
    workspace = next((settings for relative, settings in manifests.items()
                      if relative.replace("\\", "/") == "apps/maple-agent/Cargo.toml"), {})
    # Resolver 1 can unify features from inactive target dependencies. Only the
    # explicitly selected modern workspace resolvers give this pruning rule.
    if str(workspace.get("resolver")) not in ("2", "3"):
        return result
    for relative, settings in manifests.items():
        manifest = tomllib.loads(manifest_path(root, relative).read_text(encoding="utf-8"))
        manifests[relative] = scoped_manifest(settings, manifest, context)
    return result


def build_document(profile: str, root: Path = agent_cache.ROOT,
                   cargo_config_dir: Path | None = None) -> dict:
    legacy = agent_cache.build_identity(profile, root, cargo_config_dir)
    canonical = canonical_identity(legacy, root)
    return {
        **canonical,
        "compatibility": agent_cache.digest(canonical),
        "legacy_compatibility": agent_cache.digest(legacy),
        "legacy_identity": legacy,
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--profile", choices=agent_cache.PROFILES, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--cargo-config-dir", type=Path)
    arguments = parser.parse_args()
    document = build_document(arguments.profile, cargo_config_dir=arguments.cargo_config_dir)
    arguments.output.parent.mkdir(parents=True, exist_ok=True)
    arguments.output.write_text(json.dumps(document, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    if os.environ.get("GITHUB_OUTPUT"):
        with Path(os.environ["GITHUB_OUTPUT"]).open("a", encoding="utf-8") as output:
            for key in ("compatibility", "legacy_compatibility"):
                output.write(f"{key}={document[key]}\n")
    print(f"Agent {arguments.profile} scoped cache compatibility: {document['compatibility']}")
    print(f"Legacy cache compatibility: {document['legacy_compatibility']}")
    print(document["rustc"]["version"])
    print(f"Native environment: {document['platform']}/{document['architecture']}; details: {arguments.output}")


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, KeyError, subprocess.SubprocessError) as error:
        print(f"Agent cache scope failed: {error}", file=sys.stderr)
        sys.exit(1)
