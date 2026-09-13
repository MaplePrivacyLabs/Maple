#!/usr/bin/env python3
"""Validate existing signed PCR files and prepare an offline legacy mirror copy.

Uses only the public verification key. Never signs, stages, commits, fetches,
or pushes. --apply copies verified source blobs; append-signature accepts a
public signature and verifies it before changing a history.
"""

import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import time

from cryptography.exceptions import InvalidSignature
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import ec, utils


FILES = ("pcrDev.json", "pcrProd.json", "pcrDevHistory.json", "pcrProdHistory.json")
MAX_BYTES = 1024 * 1024
MAX_ENTRIES = 2048
MAX_SAFE_INTEGER = 2**53 - 1
NIX_STORE = Path("/nix/store")
PUBLIC_KEY_B64 = (
    "MHYwEAYHKoZIzj0CAQYFK4EEACIDYgAEHiUY9kFWK1GqBGzczohhwEwElXzgWLDZa9R6wBx3"
    "JOBocgSt9+UIzZlJbPDjYeGBfDUXh7Z62BG2vVsh2NgclLB5S7A2ucBBtb1wd8vSQHP8jpdP"
    "hZX1slauPgbnROIP"
)
PUBLIC_KEY = serialization.load_der_public_key(base64.b64decode(PUBLIC_KEY_B64))
PCR_FIELDS = {"PCR0", "PCR1", "PCR2"}
HISTORY_FIELDS = PCR_FIELDS | {"timestamp", "signature"}


class ValidationError(ValueError):
    pass


def require(condition, message):
    if not condition:
        raise ValidationError(message)


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        require(key not in result, f"Duplicate JSON field: {key}")
        result[key] = value
    return result


def parse_json(data, label):
    require(len(data) <= MAX_BYTES, f"{label}: exceeds the SDK's 1 MiB limit")
    try:
        return json.loads(data.decode("utf-8"), object_pairs_hook=unique_object)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise ValidationError(f"{label}: invalid UTF-8 JSON") from error


def validate_pcrs(value, label):
    for field in PCR_FIELDS:
        pcr = value.get(field)
        require(
            isinstance(pcr, str)
            and re.fullmatch(r"[0-9a-f]{96}", pcr) is not None
            and pcr != "0" * 96,
            f"{label}: {field} must be 96 nonzero lowercase hexadecimal characters",
        )


def validate_history(data, label):
    history = parse_json(data, label)
    require(
        isinstance(history, list) and 0 < len(history) <= MAX_ENTRIES,
        f"{label}: history must contain 1..{MAX_ENTRIES} entries",
    )
    seen = set()
    for index, entry in enumerate(history):
        row = f"{label}[{index}]"
        require(isinstance(entry, dict) and set(entry) == HISTORY_FIELDS, f"{row}: invalid fields")
        validate_pcrs(entry, row)
        require(entry["PCR0"] not in seen, f"{row}: duplicate PCR0")
        seen.add(entry["PCR0"])
        timestamp = entry["timestamp"]
        require(
            type(timestamp) is int and 0 < timestamp <= MAX_SAFE_INTEGER,
            f"{row}: timestamp must be a positive safe integer",
        )
        encoded = entry["signature"]
        require(isinstance(encoded, str) and len(encoded) == 128, f"{row}: invalid signature encoding")
        try:
            signature = base64.b64decode(encoded, validate=True)
        except ValueError as error:
            raise ValidationError(f"{row}: invalid signature encoding") from error
        require(len(signature) == 96, f"{row}: signature must be 96 bytes")
        der_signature = utils.encode_dss_signature(
            int.from_bytes(signature[:48], "big"), int.from_bytes(signature[48:], "big")
        )
        try:
            # The existing format authenticates PCR0 text only, not the other fields.
            PUBLIC_KEY.verify(der_signature, entry["PCR0"].encode("ascii"), ec.ECDSA(hashes.SHA384()))
        except InvalidSignature as error:
            raise ValidationError(f"{row}: PCR0 signature is invalid") from error
    return history


def validate_bundle(blobs):
    require(set(blobs) == set(FILES), "Expected exactly the four supported PCR files")
    histories = {}
    for environment in ("Dev", "Prod"):
        current_name = f"pcr{environment}.json"
        history_name = f"pcr{environment}History.json"
        current = parse_json(blobs[current_name], current_name)
        require(
            isinstance(current, dict) and set(current) == PCR_FIELDS | {"HashAlgorithm"},
            f"{current_name}: invalid fields",
        )
        require(current["HashAlgorithm"] == "Sha384 { ... }", f"{current_name}: unexpected hash algorithm")
        validate_pcrs(current, current_name)
        history = validate_history(blobs[history_name], history_name)
        require(
            any(all(entry[field] == current[field] for field in PCR_FIELDS) for entry in history),
            f"{current_name}: PCR0/1/2 do not match a signed-history entry",
        )
        histories[history_name] = history
    return histories


def validate_extension(source, baseline):
    source_histories = validate_bundle(source)
    baseline_histories = validate_bundle(baseline)
    for name, old_entries in baseline_histories.items():
        require(
            source_histories[name][: len(old_entries)] == old_entries,
            f"{name}: source truncates, reorders, or changes the legacy history; reconcile it first",
        )
    return source_histories


def read_directory(directory):
    root = Path(directory).resolve(strict=True)
    blobs = {}
    for name in FILES:
        path = root / name
        require(not path.is_symlink() and path.is_file(), f"{path}: expected a regular file, not a symlink")
        require(path.stat().st_size <= MAX_BYTES, f"{path}: exceeds the SDK's 1 MiB limit")
        with path.open("rb") as handle:
            blobs[name] = handle.read(MAX_BYTES + 1)
    return blobs


def git(repo, *arguments):
    try:
        return subprocess.run(
            ["git", "-C", str(repo), *arguments], check=True, stdout=subprocess.PIPE,
            stderr=subprocess.PIPE, timeout=30,
        ).stdout
    except (subprocess.CalledProcessError, subprocess.TimeoutExpired) as error:
        # Do not include arbitrary local git/remote output in an operator error.
        raise ValidationError(f"Git command failed: {arguments[0]}") from error


def git_text(repo, *arguments):
    return git(repo, *arguments).decode("utf-8").strip()


def check_repository(directory, expected_name):
    root = Path(directory).resolve(strict=True)
    require(git_text(root, "rev-parse", "--show-toplevel") == str(root), "Provide the exact repository worktree root")
    origin = git_text(root, "remote", "get-url", "origin")
    accepted = {
        f"https://github.com/{expected_name}", f"https://github.com/{expected_name}.git",
        f"git@github.com:{expected_name}.git", f"git@github.com:{expected_name}",
        f"ssh://git@github.com/{expected_name}.git",
    }
    require(origin in accepted, f"Expected origin to identify {expected_name}")
    return root


def check_full_ref(repo, ref):
    require(re.fullmatch(r"[0-9a-f]{40}", ref) is not None, "Provide an immutable full 40-character commit SHA")
    require(git_text(repo, "rev-parse", "--verify", f"{ref}^{{commit}}") == ref, "Commit is unavailable locally")


def read_commit(repo, ref, prefix=""):
    check_full_ref(repo, ref)
    blobs = {}
    for name in FILES:
        path = prefix + name
        tree = git_text(repo, "ls-tree", ref, "--", path)
        require(tree.startswith("100644 blob ") and tree.endswith("\t" + path), f"{path}: expected a tracked regular file")
        size = int(git_text(repo, "cat-file", "-s", f"{ref}:{path}"))
        require(size <= MAX_BYTES, f"{path}: exceeds the SDK's 1 MiB limit")
        blobs[name] = git(repo, "show", f"{ref}:{path}")
    return blobs


def prepare(source_repo, source_ref, legacy_repo, legacy_ref):
    source_root = check_repository(source_repo, "MaplePrivacyLabs/Maple")
    legacy_root = check_repository(legacy_repo, "OpenSecretCloud/opensecret")
    require(source_root != legacy_root, "Source and legacy worktrees must be different")
    check_full_ref(legacy_root, legacy_ref)
    require(git_text(legacy_root, "rev-parse", "HEAD") == legacy_ref, "Legacy HEAD differs from the reviewed legacy ref")
    require(
        git_text(legacy_root, "rev-parse", "refs/remotes/origin/master") == legacy_ref,
        "Legacy origin/master differs from the reviewed legacy ref; fetch and reconcile first",
    )
    require(not git_text(legacy_root, "status", "--porcelain", "--untracked-files=all"), "Legacy worktree/index must be clean")
    source = read_commit(source_root, source_ref, "services/opensecret/")
    baseline = read_commit(legacy_root, legacy_ref)
    require(read_directory(legacy_root) == baseline, "Legacy working files differ from the reviewed commit")
    histories = validate_extension(source, baseline)
    return source, baseline, histories


def atomic_write(path, data):
    descriptor, temporary = tempfile.mkstemp(prefix=f".{path.name}.", dir=path.parent)
    try:
        with os.fdopen(descriptor, "wb") as handle:
            handle.write(data)
            handle.flush()
            os.fsync(handle.fileno())
        os.chmod(temporary, 0o644)
        os.replace(temporary, path)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def append_signature(pcr_file, history_file, signature=None):
    """Accept a public signature, never a private key or provider credential."""
    for path in (pcr_file, history_file):
        require(path.is_file() and not path.is_symlink(), "Expected existing regular PCR files")
        require(path.stat().st_size <= MAX_BYTES, "PCR input exceeds size limit")
    current_data = pcr_file.read_bytes()
    previous = history_file.read_bytes()
    current = parse_json(current_data, "snapshot")
    require(isinstance(current, dict) and set(current) == PCR_FIELDS | {"HashAlgorithm"}, "Invalid snapshot fields")
    require(current["HashAlgorithm"] == "Sha384 { ... }", "Unexpected hash algorithm")
    validate_pcrs(current, "snapshot")
    history = validate_history(previous, "history")
    for entry in history:
        if entry["PCR0"] == current["PCR0"]:
            require(all(entry[field] == current[field] for field in PCR_FIELDS), "Existing PCR0 has different measurements")
            return False
    if signature is None:
        return True
    entry = {field: current[field] for field in PCR_FIELDS}
    entry.update(timestamp=int(time.time()), signature=signature)
    updated = (json.dumps(history + [entry], indent=2) + "\n").encode("utf-8")
    validate_history(updated, "updated history")
    require(pcr_file.read_bytes() == current_data and history_file.read_bytes() == previous, "PCR inputs changed")
    atomic_write(history_file, updated)
    return True


def copy_prepared(legacy_root, source, baseline):
    require(read_directory(legacy_root) == baseline, "Legacy PCR files changed after validation")
    written = []
    try:
        for name in FILES:
            if source[name] != baseline[name]:
                atomic_write(Path(legacy_root) / name, source[name])
                written.append(name)
        require(read_directory(legacy_root) == source, "Copied bytes differ from the verified source")
    except Exception:
        for name in written:
            atomic_write(Path(legacy_root) / name, baseline[name])
        raise


def describe(blobs, histories, baseline=None):
    return {
        "histories": {name: len(entries) for name, entries in histories.items()},
        "files": {
            name: {
                "bytes": len(data), "sha256": hashlib.sha256(data).hexdigest(),
                **({"changed": data != baseline[name]} if baseline is not None else {}),
            }
            for name, data in blobs.items()
        },
    }


def check_artifact(source_dir, source_ref, artifact_dir, sha256, environment):
    """Verify an operator-reviewed immutable handoff, without building/signing."""
    source = source_dir.resolve(strict=True)
    repo = source.parents[1]
    check_repository(repo, "MaplePrivacyLabs/Maple")
    check_full_ref(repo, source_ref)
    require(git_text(repo, "rev-parse", "HEAD") == source_ref, "Checkout differs from reviewed source ref")
    require(
        not git_text(repo, "status", "--porcelain", "--untracked-files=all", "--", "services/opensecret"),
        "Backend worktree must be clean",
    )
    blobs = read_commit(repo, source_ref, "services/opensecret/")
    require(read_directory(source) == blobs, "PCR working files differ from reviewed source ref")
    validate_bundle(blobs)
    artifact = artifact_dir.resolve(strict=True)
    require(artifact.parent == NIX_STORE, "Artifact must be an immutable Nix store output")
    require(re.fullmatch(r"[0-9a-f]{64}", sha256) is not None, "Provide reviewed EIF SHA-256")
    snapshot = artifact / "pcr.json"
    image = artifact / "image.eif"
    require(not snapshot.is_symlink() and snapshot.is_file(), "Expected regular artifact measurements")
    require(not image.is_symlink() and image.is_file(), "Expected regular EIF")
    require(snapshot.stat().st_size <= MAX_BYTES, "Artifact measurements exceed size limit")
    current = parse_json(blobs[f"pcr{environment.title()}.json"], "approved snapshot")
    require(parse_json(snapshot.read_bytes(), "artifact") == current, "Artifact measurements differ from approval")
    with image.open("rb") as stream:
        actual = hashlib.file_digest(stream, "sha256").hexdigest()
    require(actual == sha256, "EIF SHA-256 differs from reviewed handoff")
    return {"source_ref": source_ref, "artifact": str(artifact), "sha256": actual, "environment": environment}


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    check = commands.add_parser("check", help="Validate working files offline, optionally against a baseline directory")
    check.add_argument("source_dir", type=Path)
    check.add_argument("--baseline-dir", type=Path)
    append = commands.add_parser("append-signature", help="Validate and append a public signature from stdin")
    append.add_argument("pcr_file", type=Path)
    append.add_argument("history_file", type=Path)
    append.add_argument("--check", action="store_true", help="Validate inputs only; do not read stdin or write")
    artifact = commands.add_parser("artifact", help="Verify an immutable, approved EIF handoff; never build")
    artifact.add_argument("--source-dir", type=Path, required=True)
    artifact.add_argument("--source-ref", required=True)
    artifact.add_argument("--artifact-dir", type=Path, required=True)
    artifact.add_argument("--sha256", required=True)
    artifact.add_argument("--environment", choices=("dev", "prod"), required=True)
    plan = commands.add_parser("prepare", help="Validate immutable commits and preview a legacy copy offline")
    for argument in ("source-repo", "source-ref", "legacy-repo", "legacy-ref"):
        plan.add_argument("--" + argument, required=True)
    plan.add_argument("--apply", action="store_true", help="Copy verified blobs only; leave changes unstaged for review")
    args = parser.parse_args(argv)
    try:
        if args.command == "check":
            blobs = read_directory(args.source_dir)
            baseline = read_directory(args.baseline_dir) if args.baseline_dir else None
            histories = validate_extension(blobs, baseline) if baseline else validate_bundle(blobs)
            result = describe(blobs, histories, baseline)
        elif args.command == "append-signature":
            signature = None if args.check else sys.stdin.read(130).strip()
            changed = append_signature(args.pcr_file, args.history_file, signature)
            result = {"append_needed": changed} if args.check else {"appended": changed}
        elif args.command == "artifact":
            result = check_artifact(args.source_dir, args.source_ref, args.artifact_dir, args.sha256, args.environment)
        else:
            blobs, baseline, histories = prepare(args.source_repo, args.source_ref, args.legacy_repo, args.legacy_ref)
            result = describe(blobs, histories, baseline)
            result.update(source_ref=args.source_ref, legacy_ref=args.legacy_ref, applied=args.apply)
            if args.apply:
                # Recheck refs, index, worktree and prefix immediately before writes.
                require(
                    prepare(args.source_repo, args.source_ref, args.legacy_repo, args.legacy_ref)[:2] == (blobs, baseline),
                    "Inputs changed after validation",
                )
                copy_prepared(Path(args.legacy_repo).resolve(), blobs, baseline)
        print(json.dumps(result, indent=2))
    except (ValidationError, OSError) as error:
        parser.exit(1, f"PCR compatibility validation failed: {error}\n")


if __name__ == "__main__":
    main()
