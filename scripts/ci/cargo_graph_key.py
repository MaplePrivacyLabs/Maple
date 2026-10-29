#!/usr/bin/env python3
"""Hash a Cargo.lock's dependency graph and a flake.lock for an exact cache key.

agent-desktop-build.yml keys its release cache on this hash. Every third-party
package's name, version, source and checksum, every dependency edge, and the
whole flake.lock (the toolchain) are hashed. Only the versions of local
packages, the ones without a `source` (workspace members and path
dependencies), are dropped, wherever they appear: a release version bump keeps
the key, while any other change gives a new one. Cargo rebuilds local packages
anyway.

Prints `hash=<sha256>` for $GITHUB_OUTPUT. Input it cannot resolve
unambiguously fails with no output.
"""

from __future__ import annotations

import hashlib
import json
from pathlib import Path
import re
import sys

try:
    import tomllib
except ModuleNotFoundError:  # Python < 3.11
    tomllib = None

SCHEMA = "maple-cargo-graph-key/1"
# Cargo's encoding of a dependency: `name`, `name version` or `name version (source)`,
# with the version and source written only when the name alone is ambiguous.
REFERENCE = re.compile(r"(?P<name>[^\s()]+)(?: (?P<version>[^\s()]+))?(?: \((?P<source>[^()\s]+)\))?")


class GraphError(ValueError):
    pass


def _text(entry: dict, key: str, where: str) -> str:
    value = entry[key]
    if not isinstance(value, str) or not value:
        raise GraphError(f"{where}: {key} must be a non-empty string")
    return value


def _packages(lock: dict) -> list:
    packages = lock.get("package")
    if not isinstance(packages, list) or not packages:
        raise GraphError("Cargo.lock has no [[package]] entries")
    nodes = []
    for position, entry in enumerate(packages):
        where = f"package {position}"
        if not isinstance(entry, dict) or "name" not in entry or "version" not in entry:
            raise GraphError(f"{where}: needs a name and a version")
        name, version = _text(entry, "name", where), _text(entry, "version", where)
        where = f"package {name} {version}"
        source = _text(entry, "source", where) if "source" in entry else None
        dependencies = entry.get("dependencies", [])
        if not isinstance(dependencies, list) or not all(isinstance(d, str) for d in dependencies):
            raise GraphError(f"{where}: dependencies must be a list of strings")
        # A local package is known by name alone, so two of one name cannot be told apart.
        identity = (name, "", "") if source is None else (name, version, source)
        nodes.append({"entry": entry, "name": name, "version": version, "source": source,
                      "identity": identity, "dependencies": dependencies, "where": where})
    seen: dict = {}
    for node in nodes:
        if node["identity"] in seen:
            raise GraphError(f"{node['where']} and {seen[node['identity']]['where']} share one identity")
        seen[node["identity"]] = node
    return nodes


def _without_commit(source: str) -> str:
    """A source without the `#<commit>` Cargo appends to a locked git package's source."""
    return source.split("#", 1)[0]


def _resolve(reference: str, by_name: dict, where: str) -> tuple:
    """The package a dependency names, by the lookup rules in Cargo's resolver/encode.rs.

    Cargo writes a reference's source without the `#<commit>` that a git package's own
    `source` carries, and ignores it when comparing, so this does too; the package's full
    source still goes into the hash. Sources are otherwise compared as written, which is
    stricter than Cargo's URL canonicalisation: a mismatch there fails loudly.
    """
    match = REFERENCE.fullmatch(reference)
    if not match:
        raise GraphError(f"{where}: unreadable dependency {reference!r}")
    candidates = by_name.get(match["name"], [])
    if match["version"] is None:
        # Cargo omits the version only when one version of the name is locked.
        if len({node["version"] for node in candidates}) != 1:
            candidates = []
    else:
        candidates = [node for node in candidates if node["version"] == match["version"]]
    if match["source"] is not None:
        wanted = _without_commit(match["source"])
        candidates = [node for node in candidates
                      if node["source"] is not None and _without_commit(node["source"]) == wanted]
    else:
        # Cargo never writes a local package's source, so it wins when none is written.
        local = [node for node in candidates if node["source"] is None]
        candidates = local or candidates
    if len(candidates) != 1:
        raise GraphError(f"{where}: dependency {reference!r} matches {len(candidates)} packages")
    return candidates[0]["identity"]


def graph(lock: dict) -> dict:
    """The lock without local packages' versions, its dependencies resolved to packages."""
    if not isinstance(lock, dict):
        raise GraphError("Cargo.lock is not a table")
    nodes = _packages(lock)
    by_name: dict = {}
    for node in nodes:
        by_name.setdefault(node["name"], []).append(node)
    packages = []
    for node in nodes:
        record = {key: value for key, value in node["entry"].items() if key != "dependencies"}
        if node["source"] is None:
            del record["version"]
        record["dependencies"] = sorted(list(_resolve(reference, by_name, node["where"]))
                                        for reference in node["dependencies"])
        packages.append((node["identity"], record))
    # Everything else (the lock format version, [metadata], [[patch.unused]]) as written.
    normalized = {key: value for key, value in lock.items() if key != "package"}
    normalized["package"] = [record for _, record in sorted(packages, key=lambda item: item[0])]
    return normalized


def parse_cargo_lock(text: str) -> dict:
    if tomllib is None:
        raise GraphError(f"reading Cargo.lock needs Python 3.11 or later (tomllib); this is {sys.version.split()[0]}")
    try:
        return tomllib.loads(text)
    except tomllib.TOMLDecodeError as error:
        raise GraphError(f"Cargo.lock is not TOML: {error}") from None


def parse_flake_lock(text: str) -> dict:
    try:
        flake = json.loads(text)
    except json.JSONDecodeError as error:
        raise GraphError(f"flake.lock is not JSON: {error}") from None
    if not isinstance(flake, dict) or not isinstance(flake.get("nodes"), dict):
        raise GraphError("flake.lock has no nodes")
    return flake


def key(cargo_lock: str, flake_lock: str) -> str:
    """SHA-256 of the normalized Cargo.lock graph and the parsed flake.lock."""
    payload = {"schema": SCHEMA, "cargo": graph(parse_cargo_lock(cargo_lock)),
               "flake": parse_flake_lock(flake_lock)}
    try:
        canonical = json.dumps(payload, sort_keys=True, separators=(",", ":"), ensure_ascii=True)
    except TypeError as error:  # a TOML date or time, which Cargo never writes
        raise GraphError(f"Cargo.lock holds a value with no canonical form: {error}") from None
    return hashlib.sha256(canonical.encode("ascii")).hexdigest()


def main(argv: list) -> int:
    if len(argv) != 2:
        print("usage: cargo_graph_key.py CARGO_LOCK FLAKE_LOCK", file=sys.stderr)
        return 2
    try:
        texts = [Path(name).read_text(encoding="utf-8") for name in argv]
        digest = key(*texts)
    except (OSError, UnicodeError, GraphError) as error:
        print(f"::error::cargo_graph_key.py: {error}", file=sys.stderr)
        return 1
    print(f"hash={digest}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
