#!/usr/bin/env python3
"""Check the manifest's recorded Cosign outcomes with a Cosign binary.

For every vector whose `cosign` field is "accept" or "reject", runs:

    cosign verify-blob-attestation --bundle B --key K --trusted-root R \
        --use-signed-timestamps --type <statement predicateType> ARTIFACT
for DSSE bundles, or
    cosign verify-blob --bundle B --key K --trusted-root R \
        --use-signed-timestamps ARTIFACT
for messageSignature bundles,

and compares the exit status with the recorded outcome. Vectors with
`"cosign": null` are profile-only cases Cosign is not expected to judge and are
skipped. Verification uses only the given files; run it without network access
(for example under `unshare -rn`) to be sure.

usage: check-cosign.py COSIGN
"""
import base64
import json
import os
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))


def has_message_signature(bundle_path):
    with open(bundle_path, encoding="utf-8") as file:
        return "messageSignature" in json.load(file)


def predicate_type(bundle_path):
    try:
        with open(bundle_path, encoding="utf-8") as file:
            payload = base64.b64decode(json.load(file)["dsseEnvelope"]["payload"])
        return json.loads(payload)["predicateType"]
    except (ValueError, KeyError):
        # Keeps the check usable for malformed payloads, but cannot establish
        # that Cosign reached the vector's intended rejection gate.
        return "unknown"


def vector_path(vector, field):
    return os.path.join(HERE, vector[field])


def main(cosign):
    with open(os.path.join(HERE, "manifest.json"), encoding="utf-8") as file:
        manifest = json.load(file)
    mismatches = 0
    for vector in manifest["vectors"]:
        expected = vector["cosign"]
        if expected is None:
            continue
        bundle = vector_path(vector, "bundle")
        if has_message_signature(bundle):
            command = [
                cosign,
                "verify-blob",
                "--bundle",
                bundle,
                "--key",
                vector_path(vector, "key"),
                "--trusted-root",
                vector_path(vector, "trusted_root"),
                "--use-signed-timestamps",
                vector_path(vector, "artifact"),
            ]
        else:
            command = [
                cosign,
                "verify-blob-attestation",
                "--bundle",
                bundle,
                "--key",
                vector_path(vector, "key"),
                "--trusted-root",
                vector_path(vector, "trusted_root"),
                "--use-signed-timestamps",
                "--type",
                predicate_type(bundle),
                vector_path(vector, "artifact"),
            ]
        result = subprocess.run(command, capture_output=True, text=True)
        actual = "accept" if result.returncode == 0 else "reject"
        detail = (result.stderr.strip().splitlines() or [""])[-1][:160]
        status = "ok" if actual == expected else "MISMATCH"
        mismatches += actual != expected
        print(f"{status:8} {vector['id']}: recorded {expected}, cosign {actual}  {detail}")
    print(f"{mismatches} mismatches")
    return 1 if mismatches else 0


if __name__ == "__main__":
    if len(sys.argv) != 2:
        print(__doc__, file=sys.stderr)
        sys.exit(2)
    sys.exit(main(sys.argv[1]))
