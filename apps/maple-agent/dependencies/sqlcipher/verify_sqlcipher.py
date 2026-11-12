#!/usr/bin/env python3
"""Regenerate pinned SQLCipher from its signed tag and compare registry C/headers."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

from verify_sources import PROVENANCE, ROOT, require, sha256, source_packages


def run(arguments, *, cwd=None, environment=None, check=True):
    result = subprocess.run(
        arguments, cwd=cwd, env=environment, text=True, encoding="utf-8", capture_output=True, check=False
    )
    if check and result.returncode != 0:
        raise ValueError(
            f"{arguments[0]} failed ({result.returncode}): {(result.stderr or result.stdout)[-4000:]}"
        )
    return result


def verify(metadata):
    packages = source_packages(metadata)
    source = packages["sqlcipher-src"] / "sqlcipher"
    upstream = PROVENANCE["sqlcipher_upstream"]
    key = ROOT / upstream["public_key_file"]
    require(sha256(key) == upstream["public_key_sha256"], "pinned public key hash differs")

    with tempfile.TemporaryDirectory(prefix="maple-sqlcipher-source-") as temporary:
        work = Path(temporary)
        keyring = work / "gnupg"
        keyring.mkdir(mode=0o700)
        environment = dict(os.environ)
        environment.update(
            GNUPGHOME=str(keyring),
            GIT_CONFIG_NOSYSTEM="1",
            GIT_CONFIG_GLOBAL=os.devnull,
        )
        run(["gpg", "--batch", "--import", str(key)], environment=environment)
        fingerprints = run(
            ["gpg", "--batch", "--with-colons", "--fingerprint"], environment=environment
        ).stdout
        primary = next(
            (line.split(":")[9] for line in fingerprints.splitlines() if line.startswith("fpr:")),
            None,
        )
        require(primary == upstream["signer_fingerprint"], "unexpected SQLCipher primary signer")

        repository = work / "repository"
        run(["git", "init", "--quiet", "--template=", str(repository)], environment=environment)
        git = ["git", "-C", str(repository)]
        run(
            git
            + [
                "-c", "fetch.recurseSubmodules=false", "fetch", "--quiet", "--depth=1",
                upstream["repository"], f"refs/tags/{upstream['tag']}:refs/tags/{upstream['tag']}",
            ],
            environment=environment,
        )
        tag_object = run(git + ["rev-parse", upstream["tag"]], environment=environment).stdout.strip()
        commit = run(
            git + ["rev-parse", f"{upstream['tag']}^{{commit}}"], environment=environment
        ).stdout.strip()
        require(tag_object == upstream["tag_object"], "SQLCipher tag object moved")
        require(commit == upstream["commit"], "SQLCipher release commit moved")
        signature = run(
            git + ["-c", "gpg.program=gpg", "verify-tag", "--raw", upstream["tag"]],
            environment=environment,
            check=False,
        )
        status = (signature.stdout + signature.stderr).splitlines()
        require(
            any(
                line.startswith("[GNUPG:] VALIDSIG ")
                and line.split()[-1] == upstream["signer_fingerprint"]
                for line in status
            )
            and not any("[GNUPG:] REVKEYSIG " in line for line in status),
            "SQLCipher tag lacks a valid signature from the pinned non-revoked key",
        )

        tree = work / "source"
        run(git + ["worktree", "add", "--quiet", "--detach", str(tree), commit], environment=environment)
        run([str(tree / "configure")], cwd=tree, environment=environment)
        run(["make", "sqlite3.c"], cwd=tree, environment=environment)

        regenerated = (tree / "sqlite3.c").read_bytes()
        require(regenerated == (source / "sqlcipher.c").read_bytes(), "regenerated SQLCipher C differs")
        for upstream_name, packaged_name in [
            ("sqlite3.h", "sqlite3.h"),
            ("sqlite3ext.h", "sqlite3ext.h"),
            ("LICENSE.md", "LICENSE-sqlcipher"),
        ]:
            require(
                (tree / upstream_name).read_bytes() == (source / packaged_name).read_bytes(),
                f"regenerated SQLCipher file differs: {packaged_name}",
            )
        return {
            "tag": upstream["tag"],
            "tag_object": tag_object,
            "commit": commit,
            "signer_fingerprint": primary,
            "amalgamation_sha256": hashlib.sha256(regenerated).hexdigest(),
            "allowed_transformations": 0,
        }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--metadata", type=Path, required=True)
    arguments = parser.parse_args()
    print(json.dumps(verify(arguments.metadata), sort_keys=True))


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, KeyError, StopIteration) as error:
        print(f"SQLCipher regeneration failed: {error}", file=sys.stderr)
        sys.exit(1)
