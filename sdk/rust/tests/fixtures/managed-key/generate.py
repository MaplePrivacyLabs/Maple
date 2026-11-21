#!/usr/bin/env python3
"""Generate the managed-key P-384 / Rekor v2 test vectors and their manifest.

Inputs are the real bundles in `sources/` (produced by the pinned Cosign; see
README.md) and the trusted roots in `sources/`. Every other vector is derived
from them here, deterministically: Python's standard library plus the OpenSSL
command line (deterministic Ed25519 and RFC 6979 ECDSA signatures).

    python3 generate.py            write vectors/, roots/, keys/, artifacts/, manifest.json
    python3 generate.py --check    regenerate into a temporary directory and fail if
                                   anything differs from the committed files

The synthetic transparency logs ("test logs") used for re-logged entries are
signed with keys derived from fixed public strings. They exist only in
`roots/test.json` and must never be trusted for anything else.
"""

import base64
import copy
from dataclasses import dataclass
from enum import Enum
import filecmp
import hashlib
import json
import os
import shutil
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
SOURCES = os.path.join(HERE, "sources")
OUTPUTS = ("vectors", "roots", "keys", "artifacts", "manifest.json")

# Start of the synthetic logs' validity window (RFC 3339).
TEST_LOG_START = "2026-01-01T00:00:00Z"
# Synthetic Rekor v1 integration time (seconds since the epoch).
V1_INTEGRATED_TIME = 1794873600  # 2026-11-17T00:00:00Z


def b64(data):
    return base64.b64encode(data).decode()


def unb64(text):
    return base64.b64decode(text)


def sha256(data):
    return hashlib.sha256(data).digest()


def canonical(obj):
    """Compact JSON with sorted keys, as Rekor canonicalizes entry bodies."""
    return json.dumps(obj, sort_keys=True, separators=(",", ":")).encode()


def load_json(path):
    with open(path, encoding="utf-8") as file:
        return json.load(file)


def pae(payload_type, payload):
    encoded_type = payload_type.encode()
    return b"DSSEv1 %d %s %d %s" % (len(encoded_type), encoded_type, len(payload), payload)


# ---------------------------------------------------------------------------
# Synthetic transparency logs
# ---------------------------------------------------------------------------

ED25519_PKCS8_PREFIX = bytes.fromhex("302e020100300506032b657004220420")
# SEC1 ECPrivateKey for P-256 without the optional public key; OpenSSL derives it.
P256_SEC1_PREFIX = bytes.fromhex("30310201010420")
P256_SEC1_SUFFIX = bytes.fromhex("a00a06082a8648ce3d030107")
KEY_HINT_LEN = 4
SIGNATURE_LINE_PREFIX = "\u2014"  # Signed-note signature lines start with an em dash.


class RekorVersion(Enum):
    V1 = 1
    V2 = 2


class TestLog:
    """A synthetic log with a deterministic key.

    Rekor v2 test logs use Ed25519 and are identified by their signed-note key
    ID, SHA-256(origin || "\n" || 0x01 || raw public key). The Rekor v1 test
    log uses ECDSA P-256, as Rekor v1 does, and is identified by SHA-256 of
    its DER public key; its signatures use RFC 6979 deterministic nonces
    (OpenSSL 3.2 or later).
    """

    @classmethod
    def rekor_v1(cls, workdir, name, host):
        return cls(workdir, name, host, version=RekorVersion.V1)

    @classmethod
    def rekor_v2(cls, workdir, name, host):
        return cls(workdir, name, host, version=RekorVersion.V2)

    def __init__(self, workdir, name, host, *, version):
        self.host = host
        self.base_url = f"https://{host}"
        self.origin = f"{host} - 1" if version is RekorVersion.V1 else host
        self.version = version
        seed = sha256(f"sigstore-rust test vectors: {name} (test-only key, never trust)".encode())
        self.key_path = os.path.join(workdir, f"{name}.pem")
        if version is RekorVersion.V1:
            der_path = os.path.join(workdir, f"{name}.der")
            with open(der_path, "wb") as file:
                file.write(P256_SEC1_PREFIX + seed + P256_SEC1_SUFFIX)
            openssl(["ec", "-inform", "DER", "-in", der_path, "-out", self.key_path])
            self.spki = openssl(["ec", "-in", self.key_path, "-pubout", "-outform", "DER"])
            self.log_id = sha256(self.spki)
            self.key_details = "PKIX_ECDSA_P256_SHA_256"
        else:
            with open(self.key_path, "w") as file:
                file.write(
                    "-----BEGIN PRIVATE KEY-----\n" + b64(ED25519_PKCS8_PREFIX + seed)
                    + "\n-----END PRIVATE KEY-----\n"
                )
            self.spki = openssl(["pkey", "-in", self.key_path, "-pubout", "-outform", "DER"])
            self.log_id = sha256(host.encode() + b"\n\x01" + self.spki[-32:])
            self.key_details = "PKIX_ED25519"

    def sign(self, message):
        with tempfile.NamedTemporaryFile(delete=False) as file:
            file.write(message)
            path = file.name
        try:
            args = ["pkeyutl", "-sign", "-inkey", self.key_path, "-rawin", "-in", path]
            if self.version is RekorVersion.V1:
                args += ["-digest", "sha256", "-pkeyopt", "nonce-type:1"]
            return openssl(args)
        finally:
            os.unlink(path)

    def signature_line(self, note_body, name=None):
        sig = self.log_id[:KEY_HINT_LEN] + self.sign(note_body.encode())
        return f"{SIGNATURE_LINE_PREFIX} {name or self.host} {b64(sig)}\n"

    def trusted_root_entry(self):
        return {
            "baseUrl": self.base_url,
            "hashAlgorithm": "SHA2_256",
            "publicKey": {
                "rawBytes": b64(self.spki),
                "keyDetails": self.key_details,
                "validFor": {"start": TEST_LOG_START},
            },
            "logId": {"keyId": b64(self.log_id)},
        }


def openssl(args):
    return subprocess.run(["openssl", *args], check=True, capture_output=True).stdout


FILLER_LEAF = b"sigstore-rust test vectors: filler leaf 0"


def leaf_hash(data):
    return sha256(b"\x00" + data)


def tree_for(body):
    """A two-leaf tree whose second leaf is `body`: (root, proof hashes)."""
    left = leaf_hash(FILLER_LEAF)
    root = sha256(b"\x01" + left + leaf_hash(body))
    return root, [left]


def note_body(origin, size, root):
    return f"{origin}\n{size}\n{b64(root)}\n"


def inclusion_proof_for(body, log, *, signers=None, origin=None):
    """A two-leaf proof and signed checkpoint for `body` at index 1."""
    root, hashes = tree_for(body)
    text = note_body(origin or log.origin, 2, root)
    lines = "".join(
        signer.signature_line(text, name) for signer, name in (signers or [(log, log.host)])
    )
    return {
        "logIndex": "1",
        "rootHash": b64(root),
        "treeSize": "2",
        "hashes": [b64(proof_hash) for proof_hash in hashes],
        "checkpoint": {"envelope": text + "\n" + lines},
    }


def relog(entry, body, log, *, signers=None, origin=None):
    """Put `body` at index 1 in `log` and sign the checkpoint.

    `signers` lists (log, name) pairs in signature-line order; the default is
    the log under its host name (which differs from its origin for Rekor v1).
    `origin` overrides the origin in the checkpoint body.
    """
    entry = copy.deepcopy(entry)
    entry["logIndex"] = "1"
    entry["logId"] = {"keyId": b64(log.log_id)}
    entry["canonicalizedBody"] = b64(body)
    entry["inclusionProof"] = inclusion_proof_for(body, log, signers=signers, origin=origin)
    return entry


def split_checkpoint(text):
    """Return the note, origin, and signature lines without a trailing empty line."""
    note, signatures = text.split("\n\n", 1)
    return note, note.split("\n")[0], signatures.rstrip("\n").split("\n")


def join_checkpoint(note, lines):
    return note + "\n\n" + "\n".join(lines) + "\n"


def flip_low_bit(data, index):
    mutated = bytearray(data)
    mutated[index] ^= 1
    return bytes(mutated)


# ---------------------------------------------------------------------------
# Vector bookkeeping
# ---------------------------------------------------------------------------


# Language-neutral reason codes (README.md, "Reason codes"). Every rejection
# carries one, so verifiers in other languages can assert the same reason
# without matching this library's error messages. A library message maps to
# its code by the substring below.
LIBRARY_CODES = (
    ("DSSE signature verification failed", "dsse-signature-invalid"),
    ("inclusion proof verification failed", "inclusion-proof-invalid"),
    ("bundle must have an inclusion proof", "inclusion-proof-missing"),
    ("No valid Rekor signature found on checkpoint", "checkpoint-signature-invalid"),
    ("No matching Rekor key found for checkpoint signature", "checkpoint-signature-invalid"),
    ("checkpoint has no valid signature from the entry's log", "checkpoint-signature-invalid"),
    ("does not match the entry's log", "checkpoint-origin-mismatch"),
    ("Failed to verify timestamp signature", "timestamp-signature-invalid"),
    ("CMS signer does not match any trusted TSA certificate", "timestamp-untrusted-authority"),
    ("Timestamp message hash mismatch", "timestamp-message-mismatch"),
    ("invalid type: null, expected a protobuf int64", "bundle-malformed"),
    ("SET verification failed", "inclusion-promise-invalid"),
    ("bundles must have an inclusion promise", "inclusion-promise-missing"),
    ("do not match its", "tlog-key-details-mismatch"),
    ("does not match key details", "tlog-digest-algorithm-mismatch"),
    ("digest is 32 bytes, expected 48", "tlog-digest-malformed"),
    ("artifact hash mismatch for hashedrekord entry", "tlog-digest-mismatch"),
    ("public key in Rekor entry does not match managed key", "tlog-key-mismatch"),
    ("message digest in bundle does not match artifact hash", "artifact-mismatch"),
    ("signature verification failed", "message-signature-invalid"),
    (
        "DSSE signature in bundle does not match signature in Rekor entry",
        "tlog-signature-mismatch",
    ),
)

# Profile clauses checked before library verification have their own codes. A
# profile rejection caused by library verification (MK-7 timestamp failures,
# MK-8) takes the code of the library error with checkpoint binding on.
PROFILE_CODES = {
    "only the v0.3 media type is accepted": "media-type-unsupported",
    "duplicate JSON keys are rejected": "duplicate-json-key",
    "conflicting content variants are rejected": "conflicting-oneof",
    "exactly one log entry is required": "tlog-entry-count",
    "Rekor v1 entries are rejected": "tlog-entry-kind",
    "no integrated time is allowed": "integrated-time-present",
    "no integrated time field is allowed": "integrated-time-present",
    "no inclusion promise is allowed": "inclusion-promise-present",
    "exactly one timestamp is required": "timestamp-count",
}


def accept():
    return {"result": "accept"}


def reject(error, message):
    matches = [(text, code) for text, code in LIBRARY_CODES if text in message]
    if not matches:
        raise AssertionError(f"no reason code for {message!r}")
    # The longest matching fragment is the most specific classifier ("DSSE signature
    # verification failed" vs "signature verification failed").
    longest = max(len(text) for text, _ in matches)
    codes = {code for text, code in matches if len(text) == longest}
    assert len(codes) == 1, f"no single reason code for {message!r}"
    return {"result": "reject", "error": error, "message": message, "code": codes.pop()}


def profile_reject(clause, reason):
    expected = {"result": "reject", "clause": clause, "reason": reason}
    if reason in PROFILE_CODES:
        expected["code"] = PROFILE_CODES[reason]
    return expected


class Corpus:
    def __init__(self, out):
        self.out = out
        self.vectors = []
        self.ids = set()

    def add(
        self, vector_id, bundle, *, description, category, source, parent, mutation,
        key, artifact, root, library, binding=None, profile=None, cosign, raw=None,
    ):
        """Record a vector and its outcomes in generation order.

        Binding defaults to the library outcome. Profile acceptance is inferred
        only when both library modes accept; MK-7/MK-8 rejections inherit the
        binding rejection's reason code. `raw` preserves deliberate duplicate
        JSON members that a dictionary cannot represent.
        """
        assert vector_id not in self.ids, vector_id
        assert category in ("positive", "standard-negative", "binding-negative", "profile-negative")
        self.ids.add(vector_id)
        if parent is not None:
            assert parent in self.ids, f"{vector_id}: parent {parent} must be defined first"
        path = f"vectors/{vector_id}/bundle.sigstore.json"
        os.makedirs(os.path.join(self.out, "vectors", vector_id), exist_ok=True)
        with open(os.path.join(self.out, path), "w", encoding="utf-8") as file:
            file.write(raw if raw is not None else json.dumps(bundle, indent=2) + "\n")
        binding = binding or library
        if profile is None:
            profile = accept() if library["result"] == "accept" and binding["result"] == "accept" else None
        assert profile is not None, f"{vector_id}: profile expectation required"
        if profile["result"] == "reject" and "code" not in profile:
            assert binding["result"] == "reject", f"{vector_id}: profile reason needs a library rejection"
            profile = {**profile, "code": binding["code"]}
        self.vectors.append(
            {
                "id": vector_id,
                "description": description,
                "category": category,
                "bundle": path,
                "trusted_root": f"roots/{root}.json",
                "key": f"keys/{key}.pub",
                "artifact": f"artifacts/{artifact}.bin",
                "source": source,
                "parent": parent,
                "mutation": mutation,
                "expected": {
                    "library": library,
                    "library_checkpoint_log_binding": binding,
                    "profile": profile,
                },
                "cosign": cosign,
            },
        )


DERIVED_SOURCE = {
    "kind": "derived",
    "detail": "single documented mutation of the parent, made by generate.py",
}
RELOGGED_SOURCE = {
    "kind": "derived",
    "detail": (
        "parent's log entry re-logged in a synthetic test log (roots/test.json) by "
        "generate.py"
    ),
}


@dataclass
class ParentVector:
    id: str
    bundle: dict
    key: str
    artifact: str
    root: str

    def add_mutation(self, corpus, vector_id, bundle, *, key=None, artifact=None, root=None, **details):
        """Use this parent's ID/key/artifact/root unless explicitly overridden."""
        corpus.add(
            vector_id,
            bundle,
            source=DERIVED_SOURCE,
            parent=self.id,
            key=key or self.key,
            artifact=artifact or self.artifact,
            root=root or self.root,
            **details,
        )


def log_entry(bundle):
    return bundle["verificationMaterial"]["tlogEntries"][0]


def entry_body(bundle):
    return json.loads(unb64(log_entry(bundle)["canonicalizedBody"]))


def real_source(runs, run):
    return {"kind": "real", "detail": runs[run]["provenance"]["detail"]}


def production_v1_log_id(production):
    logs = [log for log in production["tlogs"] if log["baseUrl"] == "https://rekor.sigstore.dev"]
    assert len(logs) == 1
    return logs[0]["logId"]["keyId"]


def add_relogged_vector(corpus, log, parent, vector_id, *, description):
    bundle = copy.deepcopy(parent.bundle)
    body = unb64(log_entry(bundle)["canonicalizedBody"])
    bundle["verificationMaterial"]["tlogEntries"][0] = relog(log_entry(bundle), body, log)
    corpus.add(
        vector_id,
        bundle,
        description=description,
        category="positive",
        source=RELOGGED_SOURCE,
        parent=parent.id,
        mutation="re-log the unchanged entry body at index 1 of a two-leaf tree in test log A",
        key=parent.key,
        artifact=parent.artifact,
        root="test",
        library=accept(),
        cosign="accept",
    )
    return ParentVector(
        id=vector_id,
        bundle=bundle,
        key=parent.key,
        artifact=parent.artifact,
        root="test",
    )


def add_body_mutation(corpus, log, parent, vector_id, *, change, description, mutation, library):
    bundle = copy.deepcopy(parent.bundle)
    body = entry_body(bundle)
    change(body["spec"]["hashedRekordV002"])
    bundle["verificationMaterial"]["tlogEntries"][0] = relog(log_entry(bundle), canonical(body), log)
    parent.add_mutation(
        corpus,
        vector_id,
        bundle,
        description=description,
        category="standard-negative",
        mutation=mutation + "; re-log the changed body in test log A",
        library=library,
        profile=profile_reject("MK-8", "library verification fails"),
        cosign="reject",
    )


def set_field(path, value):
    def change(spec):
        node = spec
        for key in path[:-1]:
            node = node[key]
        node[path[-1]] = value
    return change


def combine_mutations(*changes):
    def change(spec):
        for mutation in changes:
            mutation(spec)
    return change


def generate(out):
    with tempfile.TemporaryDirectory() as work:
        prepare_outputs(out)
        runs = load_runs(out)
        log_a = TestLog.rekor_v2(work, "test-log-a", "test-log-a.rekor.example.com")
        log_b = TestLog.rekor_v2(work, "test-log-b", "test-log-b.rekor.example.com")
        log_v1 = TestLog.rekor_v1(work, "test-log-v1", "test-log-v1.rekor.example.com")
        production = write_roots(out, log_a=log_a, log_b=log_b, log_v1=log_v1)
        corpus = Corpus(out)
        p384 = ParentVector(
            id="p384-rekor2-dsse-tsa",
            bundle=runs["r1"]["bundle"],
            key="r1",
            artifact="r1",
            root="production",
        )
        p384_second_key = ParentVector(
            id="p384-rekor2-dsse-tsa-second-key",
            bundle=runs["r2"]["bundle"],
            key="r2",
            artifact="r2",
            root="production",
        )
        p256 = ParentVector(
            id="p256-rekor2-dsse-tsa",
            bundle=runs["r3"]["bundle"],
            key="r3",
            artifact="r3",
            root="production",
        )
        blob_p384 = ParentVector(
            id="p384-rekor2-blob-tsa",
            bundle=runs["blob-p384"]["bundle"],
            key="blob-p384",
            artifact="blob-p384",
            root="production",
        )
        blob_p256b = ParentVector(
            id="p256-rekor2-blob-tsa",
            bundle=runs["blob-p256b"]["bundle"],
            key="blob-p256b",
            artifact="blob-p256b",
            root="production",
        )
        add_real_positives(corpus, runs, p384=p384, p384_second_key=p384_second_key, p256=p256)
        add_blob_vectors(corpus, runs, blob=blob_p384, prefix="p384", signature_donor=p256,
                         other_key=p384_second_key, other_key_id="r2",
                         native_alg="SHA2_384", other_alg="SHA2_256")
        add_blob_vectors(corpus, runs, blob=blob_p256b, prefix="p256", signature_donor=p256,
                         other_key=p256, other_key_id="r3",
                         native_alg="SHA2_256", other_alg="SHA2_384")
        add_standard_negatives(corpus, p384=p384, p384_second_key=p384_second_key, production=production)
        add_profile_negatives(corpus, p384=p384, p384_second_key=p384_second_key)
        relogged_parent = add_relogged_vectors(
            corpus, p384=p384, p384_second_key=p384_second_key, p256=p256, log=log_a
        )
        add_binding_negatives(corpus, relogged_parent=relogged_parent, log_a=log_a, log_b=log_b)
        add_rekor_v1_vector(corpus, p384=p384, log_v1=log_v1)
        add_p256_binding_vector(corpus, p256=p256, production=production)
        write_manifest(out, corpus, runs, logs=(log_a, log_b, log_v1))


def prepare_outputs(out):
    for name in OUTPUTS:
        path = os.path.join(out, name)
        if os.path.isdir(path):
            shutil.rmtree(path)
        elif os.path.exists(path):
            os.unlink(path)
    for directory in ("roots", "keys", "artifacts"):
        os.makedirs(os.path.join(out, directory))


def load_runs(out):
    runs = {}
    for run in ("r1", "r2", "r3", "blob-p384", "blob-p256b"):
        source_dir = os.path.join(SOURCES, run)
        runs[run] = {
            "bundle": load_json(os.path.join(source_dir, "bundle.sigstore.json")),
            "provenance": load_json(os.path.join(source_dir, "provenance.json")),
        }
        shutil.copyfile(os.path.join(source_dir, "key.pub"), os.path.join(out, "keys", f"{run}.pub"))
        shutil.copyfile(
            os.path.join(source_dir, "artifact.bin"),
            os.path.join(out, "artifacts", f"{run}.bin"),
        )
    return runs


def write_roots(out, *, log_a, log_b, log_v1):
    production = load_json(os.path.join(SOURCES, "trusted_root.production.json"))
    staging = load_json(os.path.join(SOURCES, "trusted_root.staging.json"))
    test_root = {
        "mediaType": production["mediaType"],
        "tlogs": [
            log_a.trusted_root_entry(),
            log_b.trusted_root_entry(),
            log_v1.trusted_root_entry(),
        ],
        "certificateAuthorities": [],
        "ctlogs": [],
        "timestampAuthorities": copy.deepcopy(production["timestampAuthorities"]),
    }
    untrusted_tsa = copy.deepcopy(production)
    untrusted_tsa["timestampAuthorities"] = copy.deepcopy(staging["timestampAuthorities"])
    for name, root in (("production", production), ("test", test_root),
                       ("production-staging-tsa", untrusted_tsa)):
        with open(os.path.join(out, "roots", f"{name}.json"), "w", encoding="utf-8") as file:
            file.write(json.dumps(root, indent=2) + "\n")
    return production


def add_real_positives(corpus, runs, *, p384, p384_second_key, p256):
    corpus.add(
        "p384-rekor2-dsse-tsa",
        p384.bundle,
        description=(
            "P-384 managed key, DSSE in-toto statement, Rekor v2 hashedrekord "
            "0.0.2 entry, RFC 3161 timestamp"
        ),
        category="positive",
        source=real_source(runs, "r1"),
        parent=None,
        mutation=None,
        key="r1",
        artifact="r1",
        root="production",
        library=accept(),
        cosign="accept",
    )
    corpus.add(
        "p384-rekor2-dsse-tsa-second-key",
        p384_second_key.bundle,
        description="Second P-384 positive: different key and statement",
        category="positive",
        source=real_source(runs, "r2"),
        parent=None,
        mutation=None,
        key="r2",
        artifact="r2",
        root="production",
        library=accept(),
        cosign="accept",
    )
    corpus.add(
        "p256-rekor2-dsse-tsa",
        p256.bundle,
        description="P-256 managed key, DSSE, Rekor v2, RFC 3161 timestamp",
        category="positive",
        source=real_source(runs, "r3"),
        parent=None,
        mutation=None,
        key="r3",
        artifact="r3",
        root="production",
        library=accept(),
        cosign="accept",
    )

    # Checkpoint witness lines are neither verified nor required.
    bundle = copy.deepcopy(p384.bundle)
    checkpoint = log_entry(bundle)["inclusionProof"]["checkpoint"]["envelope"]
    note, origin, lines = split_checkpoint(checkpoint)
    log_lines = [line for line in lines if line.split(" ")[1] == origin]
    assert len(log_lines) == 1 and len(lines) > 1, "expected one log signature plus witness lines"
    log_entry(bundle)["inclusionProof"]["checkpoint"]["envelope"] = join_checkpoint(note, log_lines)
    p384.add_mutation(
        corpus,
        "p384-checkpoint-witnesses-removed",
        bundle,
        description="Checkpoint keeps only the log's own signature line",
        category="positive",
        mutation="remove every witness cosignature line from the checkpoint",
        library=accept(),
        cosign="accept",
    )
    bundle = copy.deepcopy(p384.bundle)
    others = [line for line in lines if line not in log_lines]
    log_entry(bundle)["inclusionProof"]["checkpoint"]["envelope"] = join_checkpoint(note, others + log_lines)
    p384.add_mutation(
        corpus,
        "p384-checkpoint-log-signature-last",
        bundle,
        description="Log signature line after the witness lines",
        category="positive",
        mutation="move the log's signature line after the witness cosignature lines",
        library=accept(),
        cosign="accept",
    )



def add_blob_vectors(corpus, runs, *, blob, prefix, signature_donor, other_key, other_key_id,
                     native_alg, other_alg):
    native_digest = (lambda data: hashlib.sha256(data).digest()) if native_alg == "SHA2_256" else (lambda data: hashlib.sha384(data).digest())
    other_digest = (lambda data: hashlib.sha256(data).digest()) if other_alg == "SHA2_256" else (lambda data: hashlib.sha384(data).digest())

    corpus.add(
        f"{prefix}-rekor2-blob-tsa",
        blob.bundle,
        description=(
            f"{prefix.upper().replace('P', 'P-', 1)} managed key, messageSignature over the artifact, Rekor v2 "
            "hashedrekord 0.0.2 entry, RFC 3161 timestamp"
        ),
        category="positive",
        source=real_source(runs, blob.key),
        parent=None,
        mutation=None,
        key=blob.key,
        artifact=blob.artifact,
        root="production",
        library=accept(),
        cosign="accept",
    )
    # Wrong key: the bundle checked against a different approved key.
    bundle = copy.deepcopy(blob.bundle)
    bundle["verificationMaterial"]["publicKey"]["hint"] = (
        other_key.bundle["verificationMaterial"]["publicKey"]["hint"]
    )
    blob.add_mutation(
        corpus,
        f"{prefix}-blob-verified-with-other-key",
        bundle,
        description=(
            "Bundle checked against a different approved key (e.g. another "
            "environment's), with that key's hint"
        ),
        category="standard-negative",
        mutation=f"set publicKey.hint to the {other_key_id} key's hint and verify with the {other_key_id} key",
        key=other_key_id,
        library=reject("SignatureInvalid", "signature verification failed"),
        profile=profile_reject("MK-8", "library verification fails"),
        cosign="reject",
    )

    # Artifact that does not match the signed bytes.
    blob.add_mutation(
        corpus,
        f"{prefix}-blob-artifact-mismatch",
        blob.bundle,
        description="Bundle verified against a different artifact than the one that was signed",
        category="standard-negative",
        mutation="verify against the bytes of artifacts/r1.bin instead of the signed artifact",
        artifact="r1",
        library=reject("ArtifactMismatch", "message digest in bundle does not match artifact hash"),
        profile=profile_reject("MK-8", "library verification fails"),
        cosign="reject",
    )

    # Digest algorithm label swapped to the other algorithm's; digest bytes kept.
    bundle = copy.deepcopy(blob.bundle)
    bundle["messageSignature"]["messageDigest"]["algorithm"] = other_alg
    blob.add_mutation(
        corpus,
        f"{prefix}-blob-digest-algorithm-{'sha256' if other_alg == 'SHA2_256' else 'sha384'}",
        bundle,
        description=f"messageSignature digest labelled {other_alg}; digest bytes unchanged ({native_alg})",
        category="standard-negative",
        mutation=f"set messageDigest.algorithm to {other_alg}, keeping the {native_alg} digest",
        library=reject("ArtifactMismatch", "message digest in bundle does not match artifact hash"),
        profile=profile_reject("MK-8", "library verification fails"),
        cosign="reject",
    )

    # Digest replaced by the other algorithm's digest of the same artifact; label kept.
    blob_artifact = open(os.path.join(SOURCES, blob.key, "artifact.bin"), "rb").read()
    bundle = copy.deepcopy(blob.bundle)
    bundle["messageSignature"]["messageDigest"]["digest"] = b64(other_digest(blob_artifact))
    blob.add_mutation(
        corpus,
        f"{prefix}-blob-digest-replaced",
        bundle,
        description=f"messageSignature digest replaced by the {other_alg} of the artifact; label kept ({native_alg})",
        category="standard-negative",
        mutation=f"replace messageDigest.digest with the {other_alg} of the artifact, keeping algorithm {native_alg}",
        library=reject("ArtifactMismatch", "message digest in bundle does not match artifact hash"),
        profile=profile_reject("MK-8", "library verification fails"),
        cosign="reject",
    )

    # Signature substituted for another key's signature over another message. The
    # donor is the real P-256 DSSE bundle (same-key ECDSA is randomized, so a
    # second valid signature from the same key would verify).
    bundle = copy.deepcopy(blob.bundle)
    bundle["messageSignature"]["signature"] = signature_donor.bundle["dsseEnvelope"]["signatures"][0]["sig"]
    bundle["verificationMaterial"]["timestampVerificationData"] = copy.deepcopy(
        signature_donor.bundle["verificationMaterial"]["timestampVerificationData"]
    )
    blob.add_mutation(
        corpus,
        f"{prefix}-blob-signature-substituted",
        bundle,
        description=(
            "messageSignature replaced by another key's signature over a different "
            "message, with that signature's own timestamp; ECDSA is randomized, so "
            "a second valid signature from the same key would verify"
        ),
        category="standard-negative",
        mutation=(
            "replace the messageSignature signature and the RFC 3161 timestamp "
            "with those of p256-rekor2-dsse-tsa"
        ),
        library=reject("SignatureInvalid", "signature verification failed"),
        profile=profile_reject("MK-8", "library verification fails"),
        cosign="reject",
    )



def add_standard_negatives(corpus, *, p384, p384_second_key, production):
    bundle = copy.deepcopy(p384.bundle)
    statement = json.loads(unb64(bundle["dsseEnvelope"]["payload"]))
    pcr0 = statement["predicate"]["measurements"]["PCR0"]
    statement["predicate"]["measurements"]["PCR0"] = ("0" if pcr0[0] != "0" else "1") + pcr0[1:]
    bundle["dsseEnvelope"]["payload"] = b64(json.dumps(statement, indent=2).encode() + b"\n")
    p384.add_mutation(
        corpus,
        "p384-payload-pcr-tampered",
        bundle,
        description="Signed statement altered: one PCR0 digit changed",
        category="standard-negative",
        mutation="change the first hex digit of predicate.measurements.PCR0 in the DSSE payload",
        library=reject("SignatureInvalid", "DSSE signature verification failed"),
        profile=profile_reject("MK-8", "library verification fails"),
        cosign="reject",
    )

    bundle = copy.deepcopy(p384.bundle)
    bundle["dsseEnvelope"]["signatures"][0]["sig"] = p384_second_key.bundle["dsseEnvelope"]["signatures"][0]["sig"]
    bundle["verificationMaterial"]["timestampVerificationData"] = copy.deepcopy(
        p384_second_key.bundle["verificationMaterial"]["timestampVerificationData"]
    )
    p384.add_mutation(
        corpus,
        "p384-signature-substituted",
        bundle,
        description=(
            "DSSE signature replaced by another key's valid signature over "
            "another statement, with that signature's own timestamp"
        ),
        category="standard-negative",
        mutation=(
            "replace the DSSE signature and the RFC 3161 timestamp with those of "
            "p384-rekor2-dsse-tsa-second-key"
        ),
        library=reject("SignatureInvalid", "DSSE signature verification failed"),
        profile=profile_reject("MK-8", "library verification fails"),
        cosign="reject",
    )

    bundle = copy.deepcopy(p384.bundle)
    bundle["verificationMaterial"]["publicKey"]["hint"] = (
        p384_second_key.bundle["verificationMaterial"]["publicKey"]["hint"]
    )
    p384.add_mutation(
        corpus,
        "p384-verified-with-other-key",
        bundle,
        description=(
            "Bundle checked against a different approved key (e.g. another "
            "environment's), with that key's hint"
        ),
        category="standard-negative",
        mutation="set publicKey.hint to the second key's hint and verify with the second key",
        key="r2",
        library=reject("SignatureInvalid", "DSSE signature verification failed"),
        profile=profile_reject("MK-8", "library verification fails"),
        cosign="reject",
    )

    bundle = copy.deepcopy(p384.bundle)
    proof = log_entry(bundle)["inclusionProof"]
    proof["hashes"][0] = b64(flip_low_bit(unb64(proof["hashes"][0]), 0))
    p384.add_mutation(
        corpus,
        "p384-inclusion-proof-corrupted",
        bundle,
        description="One inclusion proof hash altered",
        category="standard-negative",
        mutation="flip the low bit of the first byte of inclusionProof.hashes[0]",
        library=reject("TransparencyLog.InclusionProof", "inclusion proof verification failed"),
        profile=profile_reject("MK-8", "library verification fails"),
        cosign="reject",
    )

    bundle = copy.deepcopy(p384.bundle)
    del log_entry(bundle)["inclusionProof"]
    p384.add_mutation(
        corpus,
        "p384-inclusion-proof-removed",
        bundle,
        description="The only log entry has no inclusion proof",
        category="standard-negative",
        mutation="remove inclusionProof from the log entry",
        library=reject("Bundle", "bundle must have an inclusion proof"),
        profile=profile_reject("MK-8", "library verification fails"),
        cosign="reject",
    )

    bundle = copy.deepcopy(p384.bundle)
    checkpoint = log_entry(bundle)["inclusionProof"]["checkpoint"]["envelope"]
    note, origin, lines = split_checkpoint(checkpoint)
    new_lines = []
    for line in lines:
        if line and line.split(" ")[1] == origin:
            signature = flip_low_bit(unb64(line.split(" ")[2]), -1)
            line = f"{SIGNATURE_LINE_PREFIX} {origin} {b64(signature)}"
        new_lines.append(line)
    log_entry(bundle)["inclusionProof"]["checkpoint"]["envelope"] = join_checkpoint(note, new_lines)
    p384.add_mutation(
        corpus,
        "p384-checkpoint-signature-corrupted",
        bundle,
        description="The log's checkpoint signature altered",
        category="standard-negative",
        mutation="flip the low bit of the last byte of the log's checkpoint signature",
        library=reject(
            "TransparencyLog.Checkpoint",
            "No valid Rekor signature found on checkpoint",
        ),
        binding=reject(
            "TransparencyLog.Checkpoint",
            "checkpoint has no valid signature from the entry's log",
        ),
        profile=profile_reject("MK-8", "library verification fails"),
        cosign="reject",
    )

    bundle = copy.deepcopy(p384.bundle)
    checkpoint = log_entry(bundle)["inclusionProof"]["checkpoint"]["envelope"]
    note, origin, lines = split_checkpoint(checkpoint)
    kept = [line for line in lines if line.split(" ")[1] != origin]
    log_entry(bundle)["inclusionProof"]["checkpoint"]["envelope"] = join_checkpoint(note, kept)
    p384.add_mutation(
        corpus,
        "p384-checkpoint-log-signature-removed",
        bundle,
        description="Checkpoint carries only witness cosignatures",
        category="standard-negative",
        mutation=(
            "remove the log's own signature line from the checkpoint, keeping the "
            "witness lines"
        ),
        library=reject(
            "TransparencyLog.Checkpoint",
            "No matching Rekor key found for checkpoint signature",
        ),
        binding=reject(
            "TransparencyLog.Checkpoint",
            "checkpoint has no valid signature from the entry's log",
        ),
        profile=profile_reject("MK-8", "library verification fails"),
        cosign="reject",
    )

    bundle = copy.deepcopy(p384.bundle)
    checkpoint = log_entry(bundle)["inclusionProof"]["checkpoint"]["envelope"]
    checkpoint_lines = checkpoint.split("\n")
    checkpoint_lines[2] = b64(flip_low_bit(unb64(checkpoint_lines[2]), 0))
    log_entry(bundle)["inclusionProof"]["checkpoint"]["envelope"] = "\n".join(checkpoint_lines)
    p384.add_mutation(
        corpus,
        "p384-checkpoint-root-hash-altered",
        bundle,
        description="Checkpoint root hash altered (signature no longer covers it)",
        category="standard-negative",
        mutation="flip the low bit of the first byte of the checkpoint root hash",
        library=reject("TransparencyLog.InclusionProof", "inclusion proof verification failed"),
        profile=profile_reject("MK-8", "library verification fails"),
        cosign="reject",
    )

    bundle = copy.deepcopy(p384.bundle)
    log_entry(bundle)["logId"] = {"keyId": production_v1_log_id(production)}
    p384.add_mutation(
        corpus,
        "p384-entry-names-another-trusted-log",
        bundle,
        description=(
            "Entry's log ID points at another log in the trusted root (Rekor v1 "
            "production); the checkpoint is still the real one from log2025-1"
        ),
        category="binding-negative",
        mutation="set logId.keyId to the trusted root's rekor.sigstore.dev log ID",
        library=accept(),
        binding=reject(
            "TransparencyLog.Checkpoint",
            "does not match the entry's log \"rekor.sigstore.dev\"",
        ),
        profile=profile_reject("MK-8", "checkpoint is not from the entry's log"),
        cosign="reject",
    )

    bundle = copy.deepcopy(p384.bundle)
    timestamp = bundle["verificationMaterial"]["timestampVerificationData"]["rfc3161Timestamps"][0]
    timestamp["signedTimestamp"] = b64(flip_low_bit(unb64(timestamp["signedTimestamp"]), -1))
    p384.add_mutation(
        corpus,
        "p384-timestamp-tampered",
        bundle,
        description="RFC 3161 timestamp token altered",
        category="standard-negative",
        mutation="flip the low bit of the last byte of the timestamp token (its signature)",
        library=reject(
            "Timestamp.Invalid",
            (
                "Failed to verify timestamp signature: Verification error: "
                "ECDSA_P384_SHA256 signature invalid"
            ),
        ),
        profile=profile_reject("MK-7", "the timestamp does not verify"),
        cosign="reject",
    )

    p384.add_mutation(
        corpus,
        "p384-timestamp-untrusted-authority",
        p384.bundle,
        description=(
            "Unmodified bundle checked against a trusted root whose timestamp "
            "authority is a different one (Sigstore staging)"
        ),
        category="standard-negative",
        mutation="trusted root only: replace timestampAuthorities with the staging root's",
        root="production-staging-tsa",
        library=reject(
            "Timestamp.Invalid",
            "CMS signer does not match any trusted TSA certificate",
        ),
        profile=profile_reject("MK-7", "the timestamp does not verify"),
        cosign="reject",
    )

    bundle = copy.deepcopy(p384.bundle)
    bundle["verificationMaterial"]["timestampVerificationData"] = copy.deepcopy(
        p384_second_key.bundle["verificationMaterial"]["timestampVerificationData"]
    )
    p384.add_mutation(
        corpus,
        "p384-timestamp-from-other-signature",
        bundle,
        description="A valid timestamp from the same authority, but over a different signature",
        category="standard-negative",
        mutation="replace the RFC 3161 timestamp with p384-rekor2-dsse-tsa-second-key's",
        library=reject("Timestamp.Invalid", "Timestamp message hash mismatch"),
        profile=profile_reject("MK-7", "the timestamp does not verify"),
        cosign="reject",
    )


def add_profile_negatives(corpus, *, p384, p384_second_key):
    bundle = copy.deepcopy(p384.bundle)
    bundle["verificationMaterial"]["timestampVerificationData"] = {"rfc3161Timestamps": []}
    p384.add_mutation(
        corpus,
        "p384-timestamp-removed",
        bundle,
        description="No RFC 3161 timestamp",
        category="profile-negative",
        mutation="empty timestampVerificationData.rfc3161Timestamps",
        library=accept(),
        profile=profile_reject("MK-7", "exactly one timestamp is required"),
        cosign="reject",
    )

    bundle = copy.deepcopy(p384.bundle)
    timestamps = bundle["verificationMaterial"]["timestampVerificationData"]["rfc3161Timestamps"]
    timestamps.append(copy.deepcopy(timestamps[0]))
    p384.add_mutation(
        corpus,
        "p384-timestamp-duplicated",
        bundle,
        description="The same timestamp twice",
        category="profile-negative",
        mutation="append a second copy of the RFC 3161 timestamp",
        library=accept(),
        profile=profile_reject("MK-7", "exactly one timestamp is required"),
        cosign=None,
    )

    bundle = copy.deepcopy(p384.bundle)
    bundle["verificationMaterial"]["tlogEntries"].append(copy.deepcopy(log_entry(p384.bundle)))
    p384.add_mutation(
        corpus,
        "p384-tlog-entry-duplicated",
        bundle,
        description="The same log entry twice",
        category="profile-negative",
        mutation="append a second copy of the log entry",
        library=accept(),
        profile=profile_reject("MK-5", "exactly one log entry is required"),
        cosign=None,
    )

    bundle = copy.deepcopy(p384.bundle)
    extra = copy.deepcopy(log_entry(p384.bundle))
    del extra["inclusionProof"]
    bundle["verificationMaterial"]["tlogEntries"].append(extra)
    p384.add_mutation(
        corpus,
        "p384-extra-entry-without-proof",
        bundle,
        description=(
            "A second, proofless copy of the log entry (the library requires a "
            "proof on at least one entry, not on each)"
        ),
        category="profile-negative",
        mutation="append a copy of the log entry without its inclusion proof",
        library=accept(),
        profile=profile_reject("MK-5", "exactly one log entry is required"),
        cosign=None,
    )

    bundle = copy.deepcopy(p384.bundle)
    log_entry(bundle)["integratedTime"] = str(V1_INTEGRATED_TIME)
    p384.add_mutation(
        corpus,
        "p384-rekor2-integrated-time-added",
        bundle,
        description="Rekor v2 entry carrying an (unauthenticated) integratedTime",
        category="profile-negative",
        mutation=f"add integratedTime {V1_INTEGRATED_TIME} to the log entry",
        library=accept(),
        profile=profile_reject("MK-6", "no integrated time is allowed"),
        cosign=None,
    )

    # The field must be absent, whatever its value.
    for suffix, value, shown, library in (
            ("zero-string", "0", "\"0\"", accept()),
            ("zero-number", 0, "0", accept()),
            ("null", None, "null", reject("Parse", "invalid type: null, expected a protobuf int64"))):
        bundle = copy.deepcopy(p384.bundle)
        log_entry(bundle)["integratedTime"] = value
        p384.add_mutation(
            corpus,
            f"p384-rekor2-integrated-time-{suffix}",
            bundle,
            description=f"Rekor v2 entry carrying integratedTime {shown}: present, though zero or null",
            category="profile-negative",
            mutation=f"add integratedTime {shown} to the log entry",
            library=library,
            profile=profile_reject("MK-6", "no integrated time field is allowed"),
            cosign=None,
        )

    bundle = copy.deepcopy(p384.bundle)
    log_entry(bundle)["inclusionPromise"] = {"signedEntryTimestamp": b64(b"\x30\x00")}
    p384.add_mutation(
        corpus,
        "p384-rekor2-inclusion-promise-added",
        bundle,
        description="Rekor v2 entry carrying an inclusion promise (SET)",
        category="profile-negative",
        mutation="add an inclusionPromise with an empty signature to the log entry",
        library=reject("TransparencyLog.InclusionPromise", "SET verification failed"),
        profile=profile_reject("MK-6", "no inclusion promise is allowed"),
        cosign=None,
    )

    bundle = copy.deepcopy(p384.bundle)
    bundle["mediaType"] = "application/vnd.dev.sigstore.bundle+json;version=0.2"
    p384.add_mutation(
        corpus,
        "p384-media-type-v0.2",
        bundle,
        description="Media type relabelled as bundle v0.2",
        category="profile-negative",
        mutation="set mediaType to the v0.2 media type",
        library=accept(),
        profile=profile_reject("MK-1", "only the v0.3 media type is accepted"),
        cosign=None,
    )

    bundle = copy.deepcopy(p384.bundle)
    del bundle["mediaType"]
    p384.add_mutation(
        corpus,
        "p384-media-type-missing",
        bundle,
        description="No media type",
        category="profile-negative",
        mutation="remove mediaType",
        library=reject("Bundle", "bundles must have an inclusion promise"),
        profile=profile_reject("MK-1", "only the v0.3 media type is accepted"),
        cosign=None,
    )

    text = json.dumps(p384.bundle, indent=2) + "\n"
    signed_env = (
        "  \"dsseEnvelope\": "
        + json.dumps(p384.bundle["dsseEnvelope"], indent=2).replace("\n", "\n  ")
    )
    assert signed_env in text
    dup = text.replace(
        signed_env,
        signed_env + ",\n  \"dsseEnvelope\": " + json.dumps(p384_second_key.bundle["dsseEnvelope"]),
        1,
    )
    assert dup != text
    p384.add_mutation(
        corpus,
        "p384-duplicate-dsse-envelope-key",
        None,
        raw=dup,
        description=(
            "Bundle JSON with the dsseEnvelope member twice: the signed envelope "
            "first, another key's envelope and statement second. This library "
            "keeps the first; a parser that keeps the last would read the other "
            "statement"
        ),
        category="profile-negative",
        mutation=(
            "insert a second dsseEnvelope member (the second key's envelope) after "
            "the original one"
        ),
        library=accept(),
        profile=profile_reject("MK-4", "duplicate JSON keys are rejected"),
        cosign=None,
    )

    bundle = copy.deepcopy(p384.bundle)
    bundle["messageSignature"] = {
        "messageDigest": {"algorithm": "SHA2_256", "digest": b64(sha256(b''))},
        "signature": p384.bundle["dsseEnvelope"]["signatures"][0]["sig"],
    }
    p384.add_mutation(
        corpus,
        "p384-conflicting-content",
        bundle,
        description="Both a DSSE envelope and a message signature",
        category="profile-negative",
        mutation="add a messageSignature member beside dsseEnvelope",
        library=accept(),
        profile=profile_reject("MK-4", "conflicting content variants are rejected"),
        cosign=None,
    )


def add_relogged_vectors(corpus, *, p384, p384_second_key, p256, log):
    first_relogged = add_relogged_vector(
        corpus,
        log,
        p384,
        "p384-relogged",
        description="The real P-384 entry, unchanged, in synthetic test log A",
    )
    third_relogged = add_relogged_vector(
        corpus,
        log,
        p256,
        "p256-relogged",
        description="The real P-256 entry, unchanged, in synthetic test log A",
    )
    p384_envelope = p384.bundle["dsseEnvelope"]
    p384_pae = pae(p384_envelope["payloadType"], unb64(p384_envelope["payload"]))
    p256_envelope = p256.bundle["dsseEnvelope"]
    p256_pae = pae(p256_envelope["payloadType"], unb64(p256_envelope["payload"]))
    add_body_mutation(
        corpus,
        log,
        first_relogged,
        "p384-key-details-p256",
        change=set_field(["signature", "verifier", "keyDetails"], "PKIX_ECDSA_P256_SHA_256"),
        description="P-384 key logged with P-256 key details",
        mutation="set keyDetails to PKIX_ECDSA_P256_SHA_256",
        library=reject(
            "TransparencyLog.EntryMismatch",
            "key details PKIX_ECDSA_P256_SHA_256 do not match its EcdsaP384 key",
        ),
    )
    add_body_mutation(
        corpus,
        log,
        third_relogged,
        "p256-key-labelled-p384",
        change=combine_mutations(
            set_field(["signature", "verifier", "keyDetails"], "PKIX_ECDSA_P384_SHA_384"),
            set_field(["data", "algorithm"], "SHA2_384"),
            set_field(["data", "digest"], b64(hashlib.sha384(p256_pae).digest())),
        ),
        description=(
            "Wrong curve: a P-256 key logged as P-384, with an otherwise "
            "consistent SHA-384 digest of the PAE"
        ),
        mutation=(
            "set keyDetails to PKIX_ECDSA_P384_SHA_384, algorithm to SHA2_384 and "
            "digest to SHA-384(PAE)"
        ),
        library=reject(
            "TransparencyLog.EntryMismatch",
            "key details PKIX_ECDSA_P384_SHA_384 do not match its EcdsaP256 key",
        ),
    )
    add_body_mutation(
        corpus,
        log,
        first_relogged,
        "p384-digest-algorithm-sha256",
        change=combine_mutations(
            set_field(["data", "algorithm"], "SHA2_256"),
            set_field(["data", "digest"], b64(sha256(p384_pae))),
        ),
        description="P-384 entry carrying SHA-256 of the PAE",
        mutation="set algorithm to SHA2_256 and digest to SHA-256(PAE)",
        library=reject(
            "TransparencyLog.EntryMismatch",
            (
                "digest algorithm SHA2_256 does not match key details "
                "PKIX_ECDSA_P384_SHA_384"
            ),
        ),
    )
    add_body_mutation(
        corpus,
        log,
        first_relogged,
        "p384-digest-truncated",
        change=set_field(["data", "digest"], b64(hashlib.sha384(p384_pae).digest()[:32])),
        description="SHA2_384 digest truncated to 32 bytes",
        mutation="truncate the logged digest to its first 32 bytes",
        library=reject(
            "TransparencyLog.MalformedEntry",
            "SHA2_384 digest is 32 bytes, expected 48",
        ),
    )
    flipped = flip_low_bit(hashlib.sha384(p384_pae).digest(), 0)
    add_body_mutation(
        corpus,
        log,
        first_relogged,
        "p384-digest-altered",
        change=set_field(["data", "digest"], b64(flipped)),
        description="Logged digest altered",
        mutation="flip the low bit of the first byte of the logged digest",
        library=reject("ArtifactMismatch", "artifact hash mismatch for hashedrekord entry"),
    )
    add_body_mutation(
        corpus,
        log,
        first_relogged,
        "p384-digest-of-payload",
        change=set_field(
            ["data", "digest"],
            b64(hashlib.sha384(unb64(p384_envelope["payload"])).digest()),
        ),
        description="SHA-384 of the payload alone instead of the PAE",
        mutation="set the logged digest to SHA-384(payload)",
        library=reject("ArtifactMismatch", "artifact hash mismatch for hashedrekord entry"),
    )
    other_p384_verifier = entry_body(p384_second_key.bundle)["spec"]["hashedRekordV002"]["signature"]["verifier"]
    other_p384_key = other_p384_verifier["publicKey"]["rawBytes"]
    add_body_mutation(
        corpus,
        log,
        first_relogged,
        "p384-logged-key-substituted",
        change=set_field(["signature", "verifier", "publicKey", "rawBytes"], other_p384_key),
        description="Entry logs a different P-384 key than the one verifying the signature",
        mutation="set the logged public key to the second key",
        library=reject(
            "TransparencyLog.EntryMismatch",
            "public key in Rekor entry does not match managed key",
        ),
    )
    add_body_mutation(
        corpus,
        log,
        first_relogged,
        "p384-logged-signature-substituted",
        change=set_field(
            ["signature", "content"],
            p384_second_key.bundle["dsseEnvelope"]["signatures"][0]["sig"],
        ),
        description="Entry logs a different signature than the bundle's",
        mutation="set the logged signature to the second bundle's signature",
        library=reject(
            "TransparencyLog.EntryMismatch",
            "DSSE signature in bundle does not match signature in Rekor entry",
        ),
    )
    return first_relogged


def add_binding_negatives(corpus, *, relogged_parent, log_a, log_b):
    bundle = copy.deepcopy(relogged_parent.bundle)
    body = unb64(log_entry(bundle)["canonicalizedBody"])
    bundle["verificationMaterial"]["tlogEntries"][0] = relog(
        log_entry(bundle), body, log_a, signers=[(log_b, log_a.host)]
    )
    relogged_parent.add_mutation(
        corpus,
        "p384-checkpoint-signed-by-other-trusted-log",
        bundle,
        description=(
            "Entry names test log A; its checkpoint (origin A) is signed only by "
            "test log B, which is also trusted"
        ),
        category="binding-negative",
        mutation="replace the checkpoint signature with one by test log B's key (B's key hint)",
        library=accept(),
        binding=reject(
            "TransparencyLog.Checkpoint",
            "checkpoint has no valid signature from the entry's log",
        ),
        profile=profile_reject("MK-8", "checkpoint is not from the entry's log"),
        cosign="reject",
    )
    bundle = copy.deepcopy(relogged_parent.bundle)
    bundle["verificationMaterial"]["tlogEntries"][0] = relog(log_entry(bundle), body, log_a, origin=log_b.origin)
    relogged_parent.add_mutation(
        corpus,
        "p384-checkpoint-origin-other-log",
        bundle,
        description="Checkpoint signed by test log A's key but with test log B's origin",
        category="binding-negative",
        mutation="write test log B's origin into the checkpoint body (signed by A)",
        library=accept(),
        binding=reject(
            "TransparencyLog.Checkpoint",
            (
                "checkpoint origin \"test-log-b.rekor.example.com\" does not match the "
                "entry's log \"test-log-a.rekor.example.com\""
            ),
        ),
        profile=profile_reject("MK-8", "checkpoint is not from the entry's log"),
        cosign="reject",
    )


def add_rekor_v1_vector(corpus, *, p384, log_v1):
    bundle = copy.deepcopy(p384.bundle)
    with open(os.path.join(SOURCES, "r1", "key.pub"), "rb") as key_file:
        key_pem = key_file.read()
    envelope = bundle["dsseEnvelope"]
    envelope_json = canonical(
        {
            "payload": envelope["payload"],
            "payloadType": envelope["payloadType"],
            "signatures": [{"keyid": "", "sig": envelope["signatures"][0]["sig"]}],
        },
    )
    v1_body = canonical(
        {
            "apiVersion": "0.0.1",
            "kind": "dsse",
            "spec": {
                "envelopeHash": {
                    "algorithm": "sha256",
                    "value": sha256(envelope_json).hex(),
                },
                "payloadHash": {
                    "algorithm": "sha256",
                    "value": sha256(unb64(envelope["payload"])).hex(),
                },
                "signatures": [
                    {
                        "signature": envelope["signatures"][0]["sig"],
                        "verifier": b64(key_pem),
                    },
                ],
            },
        },
    )
    proof = inclusion_proof_for(v1_body, log_v1)
    set_payload = canonical(
        {
            "body": b64(v1_body),
            "integratedTime": V1_INTEGRATED_TIME,
            "logID": log_v1.log_id.hex(),
            "logIndex": 1,
        },
    )
    v1_entry = {
        "logIndex": "1",
        "logId": {"keyId": b64(log_v1.log_id)},
        "kindVersion": {"kind": "dsse", "version": "0.0.1"},
        "integratedTime": str(V1_INTEGRATED_TIME),
        "inclusionPromise": {"signedEntryTimestamp": b64(log_v1.sign(set_payload))},
        "inclusionProof": proof,
        "canonicalizedBody": b64(v1_body),
    }
    bundle["verificationMaterial"]["tlogEntries"] = [v1_entry]
    corpus.add(
        "p384-rekor1-dsse-synthetic",
        bundle,
        description=(
            "The real P-384 DSSE signature and timestamp with a synthetic Rekor "
            "v1 dsse 0.0.1 entry (inclusion proof, checkpoint and SET from test "
            "log v1)"
        ),
        category="profile-negative",
        source={
            "kind": "synthetic",
            "detail": (
                "Rekor v1 entry built by generate.py; no public Rekor v1 run"
                " was made"
            ),
        },
        parent="p384-rekor2-dsse-tsa",
        mutation=(
            "replace the Rekor v2 entry with a synthetic Rekor v1 dsse 0.0.1 entry "
            "for the same envelope"
        ),
        key="r1",
        artifact="r1",
        root="test",
        library=accept(),
        profile=profile_reject("MK-5", "Rekor v1 entries are rejected"),
        cosign="accept",
    )


def add_p256_binding_vector(corpus, *, p256, production):
    bundle = copy.deepcopy(p256.bundle)
    log_entry(bundle)["logId"] = {"keyId": production_v1_log_id(production)}
    p256.add_mutation(
        corpus,
        "p256-entry-names-another-trusted-log",
        bundle,
        description=(
            "P-256 counterpart of p384-entry-names-another-trusted-log (also "
            "verifiable by releases without P-384 support)"
        ),
        category="binding-negative",
        mutation="set logId.keyId to the trusted root's rekor.sigstore.dev log ID",
        library=accept(),
        binding=reject(
            "TransparencyLog.Checkpoint",
            "does not match the entry's log \"rekor.sigstore.dev\"",
        ),
        profile=profile_reject("MK-8", "checkpoint is not from the entry's log"),
        cosign="reject",
    )


def write_manifest(out, corpus, runs, *, logs):
    manifest = {
        "description": (
            "Managed-key DSSE and messageSignature / Rekor v2 / RFC 3161 test "
            "vectors. See README.md and PROFILE.md."
        ),
        "generator": "generate.py",
        "sources": {run: runs[run]["provenance"] for run in runs},
        "test_logs": {
            log.host: {
                "origin": log.origin,
                "log_id": b64(log.log_id),
                "rekor_api": log.version.value,
            }
            for log in logs
        },
        "vectors": corpus.vectors,
    }
    with open(os.path.join(out, "manifest.json"), "w", encoding="utf-8") as file:
        file.write(json.dumps(manifest, indent=2) + "\n")
    print(f"wrote {len(corpus.vectors)} vectors")


def check():
    tmp = tempfile.mkdtemp()
    try:
        generate(tmp)
        diffs = []
        for name in OUTPUTS:
            committed, generated = os.path.join(HERE, name), os.path.join(tmp, name)
            if os.path.isdir(generated):
                comparison = filecmp.dircmp(committed, generated)
                stack = [comparison]
                while stack:
                    comparison = stack.pop()
                    changed = comparison.left_only + comparison.diff_files + comparison.funny_files
                    diffs += [os.path.join(comparison.left, filename) for filename in changed]
                    diffs += [os.path.join(comparison.right, filename) for filename in comparison.right_only]
                    for sub in comparison.subdirs.values():
                        stack.append(sub)
            elif not os.path.exists(committed) or not filecmp.cmp(committed, generated, shallow=False):
                diffs.append(committed)
        if diffs:
            print(
                "generated files differ from the committed ones:\n  " + "\n  ".join(diffs),
                file=sys.stderr,
            )
            sys.exit(1)
        print("committed vectors match a fresh generation")
    finally:
        shutil.rmtree(tmp)


if __name__ == "__main__":
    if sys.argv[1:] == ["--check"]:
        check()
    elif sys.argv[1:] == []:
        generate(HERE)
    else:
        print(__doc__, file=sys.stderr)
        sys.exit(2)
