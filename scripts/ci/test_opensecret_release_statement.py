"""Behavior of the OpenSecret release statement producer (opensecret_release_statement.py).

GitHub, Cosign, KMS and R2 are replaced by local fakes; a real Cosign-produced
bundle checks the evidence profile against actual Rekor v2 output.
"""

from __future__ import annotations

import base64
import contextlib
import hashlib
import http.server
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import unittest
from unittest import mock
import zipfile

import opensecret_release_statement as release
from test_opensecret_eif_scripts import VALID_MEASUREMENTS, release_build_candidate
from test_workflow_policy import workflows

ROOT = Path(__file__).resolve().parents[2]
FIXTURE = ROOT / "scripts/ci/fixtures/opensecret-release/cosign-v3.1.4-rekor-v2.bundle.json"
REVIEWED_DER = release.P384_SPKI_PREFIX + b"\x04" + bytes(range(1, 97))
OTHER_DER = release.P384_SPKI_PREFIX + b"\x04" + bytes(range(2, 98))
ACCOUNT = "123456789012"
KEY_ARN = f"arn:aws:kms:us-east-1:{ACCOUNT}:key/0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0"
ROLE_ARN = f"arn:aws:iam::{ACCOUNT}:role/maple-pcr-signer-dev"
R2_ACCOUNT = "e" * 32
DEV_PUBLIC_KEY = "scripts/ci/opensecret-release/dev-signing-key.pem"
UNTRACKED_PUBLIC_KEY = "scripts/ci/opensecret-release/untracked.pem"
LINKED_PUBLIC_KEY = "scripts/ci/opensecret-release/linked.pem"
# Sigstore's production Rekor v1 log, which a Rekor v2 profile must never accept.
REKOR_V1_LOG_ID = "wNI9atQGlz+VWfO6LRygH4QUfY/8W4RFwiT5i5WRgB0="


def pem(der):
    body = base64.encodebytes(der).decode()
    return f"-----BEGIN PUBLIC KEY-----\n{body}-----END PUBLIC KEY-----\n"


def b64(data):
    return base64.b64encode(data).decode()


def sha256_hex(data):
    return hashlib.sha256(data).hexdigest()


def git(*arguments, cwd):
    return subprocess.run(["git", "-c", "user.name=fixture", "-c", "user.email=fixture@example.invalid",
                           "-c", "commit.gpgsign=false", *arguments],
                          cwd=cwd, check=True, capture_output=True, text=True).stdout.strip()


# --- Bundles ----------------------------------------------------------------

# Byte strings with the shape the pinned Cosign writes. The profile reads their
# structure only; Cosign, which these tests replace, checks the cryptography.
FAKE_SIGNATURE = b"0d\x02\x30" + b"\x11" * 98  # an ECDSA-Sig-Value header, then filler
FAKE_ROOT_HASH = hashlib.sha256(b"fixture tree").digest()
FAKE_LOG_INDEX = "7"
FAKE_TREE_SIZE = "20"


def fake_timestamp_token(signature):
    """Filler around the one part the profile reads: the imprint OCTET STRING (04 20, then SHA-256)."""
    return b"\x30\x82\x02\xca" + b"\x05" * 40 + b"\x04\x20" + hashlib.sha256(signature).digest() + b"\x06" * 40


def fake_checkpoint(origin, key_id):
    """A signed note for the fake tree: the log's own signature line, then a witness cosignature."""
    log_signature = b64(key_id[:release.NOTE_KEY_HASH_BYTES] + b"\x22" * 64)
    witness_signature = b64(b"\x01\x02\x03\x04" + b"\x33" * 72)
    return (f"{origin}\n{FAKE_TREE_SIZE}\n{b64(FAKE_ROOT_HASH)}\n\n"
            f"— {origin} {log_signature}\n"
            f"— witness.example {witness_signature}\n")


def hashedrekord_body(statement):
    envelope_digest = hashlib.sha384(release.pae(release.PAYLOAD_TYPE, statement)).digest()
    return {
        "apiVersion": "0.0.2",
        "kind": "hashedrekord",
        "spec": {
            "hashedRekordV002": {
                "data": {"algorithm": "SHA2_384", "digest": b64(envelope_digest)},
                "signature": {
                    "content": b64(FAKE_SIGNATURE),
                    "verifier": {"keyDetails": "PKIX_ECDSA_P384_SHA_384", "publicKey": {"rawBytes": b64(REVIEWED_DER)}},
                },
            },
        },
    }


def make_bundle(statement):
    """A bundle with the exact shape the pinned Cosign writes for the reviewed key, without real signatures."""
    services = release.signing_services()
    log_entry = {
        "logIndex": FAKE_LOG_INDEX,
        "logId": {"keyId": b64(services["log_key_id"])},
        "kindVersion": {"kind": "hashedrekord", "version": "0.0.2"},
        "inclusionProof": {
            "logIndex": FAKE_LOG_INDEX,
            "rootHash": b64(FAKE_ROOT_HASH),
            "treeSize": FAKE_TREE_SIZE,
            "hashes": [b64(bytes([n]) * 32) for n in range(5)],
            "checkpoint": {"envelope": fake_checkpoint(services["log_origin"], services["log_key_id"])},
        },
        "canonicalizedBody": b64(json.dumps(hashedrekord_body(statement)).encode()),
    }
    return {
        "mediaType": release.BUNDLE_MEDIA_TYPE,
        "verificationMaterial": {
            "publicKey": {"hint": b64(hashlib.sha256(REVIEWED_DER).digest())},
            "tlogEntries": [log_entry],
            "timestampVerificationData": {
                "rfc3161Timestamps": [{"signedTimestamp": b64(fake_timestamp_token(FAKE_SIGNATURE))}],
            },
        },
        "dsseEnvelope": {
            "payload": b64(statement),
            "payloadType": release.PAYLOAD_TYPE,
            "signatures": [{"sig": b64(FAKE_SIGNATURE), "keyid": ""}],
        },
    }


def encode(bundle):
    return json.dumps(bundle).encode()


def log_entry(bundle):
    return bundle["verificationMaterial"]["tlogEntries"][0]


def timestamps(bundle):
    return bundle["verificationMaterial"]["timestampVerificationData"]["rfc3161Timestamps"]


def logged_body(bundle):
    return json.loads(base64.b64decode(log_entry(bundle)["canonicalizedBody"]))


@contextlib.contextmanager
def editing_logged_body(bundle):
    """Yield the decoded log entry body; the edited body is encoded back on exit."""
    body = logged_body(bundle)
    yield body
    log_entry(bundle)["canonicalizedBody"] = b64(json.dumps(body).encode())


def logged_key(bundle):
    verifier = logged_body(bundle)["spec"]["hashedRekordV002"]["signature"]["verifier"]
    return base64.b64decode(verifier["publicKey"]["rawBytes"])


# --- Bundle mutations -------------------------------------------------------
# Each is one change to a passing bundle that the profile must reject.

MUTATIONS = {}


def mutation(name):
    def register(change):
        MUTATIONS[name] = change
        return change
    return register


# Top-level shape and verification material.

@mutation("bundle v0.2")
def use_old_media_type(bundle):
    bundle["mediaType"] = "application/vnd.dev.sigstore.bundle+json;version=0.2"


@mutation("message signature")
def add_message_signature(bundle):
    bundle["messageSignature"] = {"signature": bundle["dsseEnvelope"]["signatures"][0]["sig"]}


@mutation("certificate added")
def add_certificate_chain(bundle):
    bundle["verificationMaterial"]["x509CertificateChain"] = {"certificates": [{"rawBytes": b64(b"cert")}]}


@mutation("certificate instead of key")
def replace_key_with_certificate(bundle):
    material = bundle["verificationMaterial"]
    del material["publicKey"]
    material["certificate"] = {"rawBytes": b64(b"cert")}


# DSSE envelope.

@mutation("tampered statement")
def tamper_statement(bundle):
    payload = bytearray(base64.b64decode(bundle["dsseEnvelope"]["payload"]))
    position = payload.index(b'"PCR0": "') + len(b'"PCR0": "')
    payload[position] = ord("f") if payload[position] != ord("f") else ord("e")
    bundle["dsseEnvelope"]["payload"] = b64(bytes(payload))


@mutation("two DSSE signatures")
def add_second_signature(bundle):
    signatures = bundle["dsseEnvelope"]["signatures"]
    signatures.append(dict(signatures[0]))


@mutation("unknown envelope field")
def add_envelope_field(bundle):
    bundle["dsseEnvelope"]["integratedTime"] = "0"


@mutation("unknown signature field")
def add_signature_field(bundle):
    bundle["dsseEnvelope"]["signatures"][0]["certificate"] = "MA=="


# Transparency log entries.

@mutation("two entries")
def duplicate_entry(bundle):
    bundle["verificationMaterial"]["tlogEntries"].append(dict(log_entry(bundle)))


@mutation("extra v1 entry")
def add_rekor_v1_entry(bundle):
    extra = dict(log_entry(bundle), kindVersion={"kind": "dsse", "version": "0.0.1"},
                 logId={"keyId": REKOR_V1_LOG_ID}, integratedTime="1700000000",
                 inclusionPromise={"signedEntryTimestamp": b64(b"set")})
    bundle["verificationMaterial"]["tlogEntries"].append(extra)


@mutation("Rekor v1 entry")
def downgrade_entry_version(bundle):
    log_entry(bundle)["kindVersion"] = {"kind": "hashedrekord", "version": "0.0.1"}


@mutation("v1 log")
def use_rekor_v1_log(bundle):
    log_entry(bundle)["logId"] = {"keyId": REKOR_V1_LOG_ID}


@mutation("integrated time")
def add_integrated_time(bundle):
    log_entry(bundle)["integratedTime"] = "1700000000"


@mutation("zero integrated time string")
def add_zero_integrated_time_string(bundle):
    log_entry(bundle)["integratedTime"] = "0"


@mutation("zero integrated time number")
def add_zero_integrated_time_number(bundle):
    log_entry(bundle)["integratedTime"] = 0


@mutation("empty integrated time")
def add_empty_integrated_time(bundle):
    log_entry(bundle)["integratedTime"] = ""


@mutation("null integrated time")
def add_null_integrated_time(bundle):
    log_entry(bundle)["integratedTime"] = None


@mutation("inclusion promise")
def add_inclusion_promise(bundle):
    log_entry(bundle)["inclusionPromise"] = {"signedEntryTimestamp": b64(b"set")}


@mutation("empty inclusion promise")
def add_empty_inclusion_promise(bundle):
    log_entry(bundle)["inclusionPromise"] = {}


@mutation("unknown entry field")
def add_entry_field(bundle):
    log_entry(bundle)["extension"] = {}


@mutation("log entry not an object")
def replace_entry_with_boolean(bundle):
    bundle["verificationMaterial"]["tlogEntries"][0] = True


@mutation("log ID not an object")
def replace_log_id_with_list(bundle):
    log_entry(bundle)["logId"] = [log_entry(bundle)["logId"]["keyId"]]


# Inclusion proof and checkpoint.

@mutation("no inclusion proof")
def drop_inclusion_proof(bundle):
    del log_entry(bundle)["inclusionProof"]


@mutation("no checkpoint")
def drop_checkpoint(bundle):
    del log_entry(bundle)["inclusionProof"]["checkpoint"]


@mutation("unknown proof field")
def add_proof_field(bundle):
    log_entry(bundle)["inclusionProof"]["integratedTime"] = "0"


@mutation("proof for another entry")
def prove_another_entry(bundle):
    log_entry(bundle)["inclusionProof"]["logIndex"] = str(int(log_entry(bundle)["logIndex"]) + 1)


@mutation("proof for another tree")
def prove_another_tree(bundle):
    log_entry(bundle)["inclusionProof"]["rootHash"] = b64(hashlib.sha256(b"other tree").digest())


@mutation("checkpoint from another log")
def move_checkpoint_to_another_log(bundle):
    checkpoint = log_entry(bundle)["inclusionProof"]["checkpoint"]
    checkpoint["envelope"] = checkpoint["envelope"].replace("log2025-1.rekor.sigstore.dev", "log2099-1.example.invalid")


@mutation("checkpoint without log signature")
def keep_only_witness_signature(bundle):
    checkpoint = log_entry(bundle)["inclusionProof"]["checkpoint"]
    note, _, _ = checkpoint["envelope"].partition("\n\n")
    witness_signature = b64(b"\x01\x02\x03\x04" + b"\x33" * 64)
    checkpoint["envelope"] = f"{note}\n\n— witness.example {witness_signature}\n"


# Logged hashedrekord body.

@mutation("dsse entry kind")
def log_as_dsse(bundle):
    with editing_logged_body(bundle) as body:
        body["kind"] = "dsse"


@mutation("SHA-256 digest")
def log_sha256_digest(bundle):
    payload = base64.b64decode(bundle["dsseEnvelope"]["payload"])
    with editing_logged_body(bundle) as body:
        data = body["spec"]["hashedRekordV002"]["data"]
        data["algorithm"] = "SHA2_256"
        data["digest"] = b64(hashlib.sha256(payload).digest())


@mutation("digest of payload, not PAE")
def log_payload_digest(bundle):
    payload = base64.b64decode(bundle["dsseEnvelope"]["payload"])
    with editing_logged_body(bundle) as body:
        body["spec"]["hashedRekordV002"]["data"]["digest"] = b64(hashlib.sha384(payload).digest())


@mutation("logged signature differs")
def log_other_signature(bundle):
    with editing_logged_body(bundle) as body:
        body["spec"]["hashedRekordV002"]["signature"]["content"] = b64(b"other signature")


@mutation("other signer key")
def log_other_key(bundle):
    with editing_logged_body(bundle) as body:
        body["spec"]["hashedRekordV002"]["signature"]["verifier"]["publicKey"]["rawBytes"] = b64(OTHER_DER)


@mutation("P-256 key details")
def log_p256_key_details(bundle):
    with editing_logged_body(bundle) as body:
        body["spec"]["hashedRekordV002"]["signature"]["verifier"]["keyDetails"] = "PKIX_ECDSA_P256_SHA_256"


# Logged body parts that are not JSON objects: each must fail where it is first read.

@mutation("logged spec not an object")
def replace_logged_spec_with_list(bundle):
    with editing_logged_body(bundle) as body:
        body["spec"] = [1]


@mutation("logged hashedrekord not an object")
def replace_logged_hashedrekord_with_list(bundle):
    with editing_logged_body(bundle) as body:
        body["spec"]["hashedRekordV002"] = [1]


@mutation("logged data not an object")
def replace_logged_data_with_string(bundle):
    with editing_logged_body(bundle) as body:
        body["spec"]["hashedRekordV002"]["data"] = "SHA2_384"


@mutation("logged signature not an object")
def replace_logged_signature_with_list(bundle):
    with editing_logged_body(bundle) as body:
        body["spec"]["hashedRekordV002"]["signature"] = [1]


@mutation("logged verifier not an object")
def replace_logged_verifier_with_boolean(bundle):
    with editing_logged_body(bundle) as body:
        body["spec"]["hashedRekordV002"]["signature"]["verifier"] = True


@mutation("logged public key not an object")
def replace_logged_public_key_with_list(bundle):
    with editing_logged_body(bundle) as body:
        body["spec"]["hashedRekordV002"]["signature"]["verifier"]["publicKey"] = [1]


NOT_AN_OBJECT = ("log entry not an object", "log ID not an object", "logged spec not an object",
                 "logged hashedrekord not an object", "logged data not an object", "logged signature not an object",
                 "logged verifier not an object", "logged public key not an object")


# RFC 3161 timestamps.

@mutation("no timestamp data")
def drop_timestamp_data(bundle):
    del bundle["verificationMaterial"]["timestampVerificationData"]


@mutation("no timestamp")
def drop_timestamp(bundle):
    timestamps(bundle).clear()


@mutation("two timestamps")
def duplicate_timestamp(bundle):
    timestamps(bundle).append(dict(timestamps(bundle)[0]))


@mutation("timestamp over another signature")
def timestamp_another_signature(bundle):
    timestamps(bundle)[0]["signedTimestamp"] = b64(b"\x30\x82" + b"\x04\x20" + hashlib.sha256(b"other").digest())


@mutation("unknown timestamp field")
def add_timestamp_field(bundle):
    timestamps(bundle)[0]["integratedTime"] = "0"


class BundleProfileTests(unittest.TestCase):
    def setUp(self):
        self.statement = release.statement_bytes("development", "1" * 40, VALID_MEASUREMENTS, 42, 1, "d" * 64)

    def real_bundle(self):
        """Pinned Cosign output for a synthetic statement and a throwaway P-384 key,
        logged in Sigstore's production Rekor v2 log with a public timestamp."""
        data = FIXTURE.read_bytes()
        bundle = json.loads(data)
        statement = base64.b64decode(bundle["dsseEnvelope"]["payload"])
        return data, statement, logged_key(bundle)

    def test_real_cosign_rekor_v2_bundle_meets_the_profile(self):
        data, statement, key = self.real_bundle()
        summary = release.check_bundle_profile(data, statement, key)
        self.assertEqual(summary["log"], "https://log2025-1.rekor.sigstore.dev")
        self.assertEqual(summary["timestamps"], 1)
        self.assertEqual(json.loads(statement)["predicateType"], release.PREDICATE_TYPE)

    def test_real_cosign_bundle_is_refused_for_another_reviewed_key(self):
        data, statement, _ = self.real_bundle()
        with self.assertRaisesRegex(release.EvidenceError, "reviewed signing key"):
            release.check_bundle_profile(data, statement, REVIEWED_DER)

    def test_every_single_mutation_of_the_real_bundle_fails(self):
        data, statement, key = self.real_bundle()
        for name, mutate in MUTATIONS.items():
            with self.subTest(mutation=name):
                changed = json.loads(data)
                mutate(changed)
                with self.assertRaises(release.EvidenceError):
                    release.check_bundle_profile(encode(changed), statement, key)

    def test_synthetic_bundle_meets_the_profile(self):
        release.check_bundle_profile(encode(make_bundle(self.statement)), self.statement, REVIEWED_DER)

    def test_every_single_mutation_of_the_synthetic_bundle_fails(self):
        for name, mutate in MUTATIONS.items():
            with self.subTest(mutation=name):
                bundle = make_bundle(self.statement)
                mutate(bundle)
                with self.assertRaises(release.EvidenceError):
                    release.check_bundle_profile(encode(bundle), self.statement, REVIEWED_DER)

    def test_parts_that_are_not_json_objects_fail_with_an_evidence_error(self):
        for name in NOT_AN_OBJECT:
            with self.subTest(mutation=name):
                bundle = make_bundle(self.statement)
                MUTATIONS[name](bundle)
                with self.assertRaisesRegex(release.EvidenceError, "is not a JSON object"):
                    release.check_bundle_profile(encode(bundle), self.statement, REVIEWED_DER)

    def test_signed_statement_must_be_byte_identical(self):
        reformatted = json.dumps(json.loads(self.statement)).encode()
        with self.assertRaisesRegex(release.EvidenceError, "differs from the prepared statement"):
            release.check_bundle_profile(encode(make_bundle(self.statement)), reformatted, REVIEWED_DER)

    def test_duplicate_json_keys_are_refused(self):
        data = encode(make_bundle(self.statement))
        duplicate = data.replace(b'"mediaType": ', b'"mediaType": "x", "mediaType": ', 1)
        with self.assertRaisesRegex(release.EvidenceError, "Duplicate JSON key"):
            release.check_bundle_profile(duplicate, self.statement, REVIEWED_DER)

    def test_oversized_bundle_is_refused(self):
        data = encode(make_bundle(self.statement))
        with self.assertRaisesRegex(release.EvidenceError, "size limit"):
            release.check_bundle_profile(data + b" " * release.MAX_BUNDLE_BYTES, self.statement, REVIEWED_DER)

    def test_pinned_signing_configuration_names_one_rekor_v2_log_and_one_timestamp_authority(self):
        config = json.loads(release.SIGNING_CONFIG.read_text())
        self.assertEqual([(log["url"], log["majorApiVersion"]) for log in config["rekorTlogUrls"]],
                         [("https://log2025-1.rekor.sigstore.dev", 2)])
        self.assertEqual([(tsa["url"], tsa["majorApiVersion"]) for tsa in config["tsaUrls"]],
                         [("https://timestamp.sigstore.dev/api/v1/timestamp", 1)])
        self.assertNotIn("caUrls", config)
        self.assertNotIn("oidcUrls", config)
        services = release.signing_services()
        self.assertEqual(services["log_origin"], "log2025-1.rekor.sigstore.dev")

    def test_build_modes_map_explicitly_to_signed_environments_and_fixed_hosts(self):
        self.assertEqual({mode: (target.environment, target.bucket, target.host)
                          for mode, target in release.TARGETS.items()},
                         {"dev": ("development", "maple-attestations-dev", "attestations-dev.maple.ai"),
                          "prod": ("production", "maple-attestations-prod", "attestations.maple.ai")})
        for mode in ("development", "Dev", "", "staging"):
            with self.subTest(mode=mode), self.assertRaises(release.EvidenceError):
                release.target_for(mode)

    def test_statement_is_an_enclave_release_statement_for_the_eif(self):
        statement = json.loads(self.statement)
        self.assertEqual(statement, {
            "_type": "https://in-toto.io/Statement/v1",
            "subject": [{"name": "image.eif", "digest": {"sha256": "d" * 64}}],
            "predicateType": "https://maple.ai/attestations/enclave-release/v1",
            "predicate": {
                "environment": "development",
                "source": {"repository": "https://github.com/MaplePrivacyLabs/Maple", "commit": "1" * 40},
                "measurements": {"PCR0": "a" * 96, "PCR1": "b" * 96, "PCR2": "c" * 96},
                "build": {"runId": "42", "runAttempt": 1},
            },
        })


# --- Workspace: a master checkout, the build's evidence and fake tools -------

# Each fake logs its call under STATE and answers from fixture files there. The
# paths are written into the scripts because the producer strips the environment.
FAKE_GH = r'''
import hashlib, json, sys
args = sys.argv[1:]
with (STATE / "gh.log").open("a") as log:
    log.write(json.dumps(args) + "\n")
config = json.loads((STATE / "gh.json").read_text())
if args[:2] == ["attestation", "verify"]:
    digest = hashlib.sha256(Path(args[2]).read_bytes()).hexdigest()
    found = [item for item in config["attestations"] if digest in item["digests"]]
    if not found:
        sys.exit(1)
    print(json.dumps([{"verificationResult": {"statement": item["statement"]}} for item in found]))
elif args[0] == "api" and "--paginate" in args:
    print(json.dumps(config["artifacts"]))
elif args[0] == "api" and args[1].endswith("/zip"):
    sys.stdout.buffer.write((STATE / "artifact.zip").read_bytes())
elif args[0] == "api":
    print(json.dumps(config["run"]))
else:
    sys.exit("unexpected gh call")
'''

FAKE_COSIGN = r'''
import json, os, shutil, sys
args = sys.argv[1:]
env = {key: value for key, value in os.environ.items() if key.startswith(("AWS_", "TUF_"))}
token = env.get("AWS_WEB_IDENTITY_TOKEN_FILE")
with (STATE / "cosign.log").open("a") as log:
    log.write(json.dumps({"args": args, "env": env,
                          "token": Path(token).read_text() if token and Path(token).exists() else None}) + "\n")
config = json.loads((STATE / "cosign.json").read_text()) if (STATE / "cosign.json").exists() else {}
if args[0] == "public-key":
    sys.stdout.write((STATE / "served.pem").read_text())
elif args[0] == "attest-blob":
    if config.get("attest_fails"):
        sys.exit(1)
    shutil.copyfile(STATE / "signed-bundle.json", args[args.index("--bundle") + 1])
elif args[0] == "verify-blob-attestation":
    sys.exit(1 if config.get("verify_fails") else 0)
else:
    sys.exit("unexpected cosign call")
'''

FAKE_AWS = r'''
import json, os, sys
args = sys.argv[1:]
with (STATE / "aws.log").open("a") as log:
    log.write(json.dumps({"args": args, "env": sorted(key for key in os.environ if key.startswith("AWS_"))}) + "\n")
if (STATE / "aws-fails").exists():
    sys.exit(1)
assert args[:2] == ["s3api", "put-object"], args


def option(name):
    return args[args.index(name) + 1]


stored = STORAGE / option("--bucket") / option("--key")
stored.parent.mkdir(parents=True, exist_ok=True)
stored.write_bytes(Path(option("--body")).read_bytes())
metadata = {"type": option("--content-type"), "cache": option("--cache-control")}
Path(str(stored) + ".meta").write_text(json.dumps(metadata))
'''


def install_fake(path, source, **paths):
    """Write an executable Python script; each keyword becomes a Path constant at its top."""
    constants = "".join(f"{name} = Path({str(value)!r})\n" for name, value in paths.items())
    path.write_text(f"#!{sys.executable}\nfrom pathlib import Path\n{constants}{source}")
    path.chmod(0o755)


@contextlib.contextmanager
def workspace():
    """A Workspace whose patches of the producer and the process environment end with it."""
    with tempfile.TemporaryDirectory() as temporary, contextlib.ExitStack() as patches:
        work = Workspace(Path(temporary).resolve())
        for patch in work.patches():
            patches.enter_context(patch)
        yield work


class Workspace:
    """A master checkout, the dev build's evidence as GitHub serves it, and fakes for gh, Cosign and aws."""

    def __init__(self, root):
        self.root = root
        self.run_id = 4242
        self.candidate = root / "candidate"
        self.release = root / "release"
        self.create_master_checkout()
        self.describe_build()
        self.install_fakes()

    def create_master_checkout(self):
        """The build's commit, then a later master commit that adds the reviewed public key."""
        self.repo = self.root / "repo"
        self.repo.mkdir()
        git("init", "-q", "--initial-branch=master", cwd=self.repo)
        (self.repo / "source").write_text("build source\n")
        self.commit("build")
        self.source_sha = git("rev-parse", "HEAD", cwd=self.repo)
        (self.repo / "later").write_text("later master change\n")
        key = self.repo / DEV_PUBLIC_KEY
        key.parent.mkdir(parents=True)
        key.write_text(pem(REVIEWED_DER))
        self.commit("later")

    def commit(self, message):
        git("add", "-A", cwd=self.repo)
        git("commit", "-q", "-m", message, cwd=self.repo)

    def describe_build(self):
        """The build's files, run, artifact and provenance. Tests change these before staging them."""
        self.files = {
            "image.eif": b"fixture EIF, not a real image",
            "pcr.json": json.dumps(VALID_MEASUREMENTS).encode(),
        }
        self.handoff = {
            "environment": "dev",
            "source_sha": self.source_sha,
            "measurements": VALID_MEASUREMENTS,
            "eif_sha256": sha256_hex(self.files["image.eif"]),
            "approval_needed": True,
        }
        self.run = {
            "id": self.run_id,
            "run_attempt": 1,
            "path": release.BUILD_WORKFLOW,
            "event": "workflow_dispatch",
            "head_branch": "master",
            "head_sha": self.source_sha,
            "status": "completed",
            "conclusion": "success",
            "repository": {"full_name": release.REPOSITORY, "private": False},
            "head_repository": {"full_name": release.REPOSITORY},
        }
        self.artifact = {
            "id": 99,
            "name": f"opensecret-eif-dev-{self.source_sha[:12]}",
            "expired": False,
            "size_in_bytes": 4096,
            "workflow_run": {"id": self.run_id, "head_sha": self.source_sha},
        }
        # The build provenance covers every file in SHA256SUMS (or attested_names),
        # from this attempt of this run.
        self.attested_names = None
        self.attested_run_id = self.run_id
        self.attested_attempt = 1
        self.extra_zip_entries = {}

    def install_fakes(self):
        self.bin = self.root / "bin"
        self.state = self.root / "state"
        self.storage = self.root / "storage"
        for directory in (self.bin, self.state, self.storage):
            directory.mkdir()
        install_fake(self.bin / "gh", FAKE_GH, STATE=self.state)
        install_fake(self.bin / "cosign", FAKE_COSIGN, STATE=self.state)
        install_fake(self.bin / "aws", FAKE_AWS, STATE=self.state, STORAGE=self.storage)
        (self.state / "served.pem").write_text(pem(REVIEWED_DER))

    def patches(self):
        """The job's environment, this checkout as ROOT and the fake Cosign as the pinned release."""
        environ = {
            "PATH": f"{self.bin}:{os.environ['PATH']}", "HOME": str(self.root), "TMPDIR": str(self.root),
            "SIGNING_KEY_ARN": KEY_ARN, "SIGNER_ROLE_ARN": ROLE_ARN, "SIGNING_PUBLIC_KEY": DEV_PUBLIC_KEY,
            "GITHUB_RUN_ID": "777", "GITHUB_OUTPUT": str(self.root / "github-output"),
            "GITHUB_STEP_SUMMARY": str(self.root / "summary.md"),
            "ATTESTATIONS_R2_ACCOUNT_ID": R2_ACCOUNT, "ATTESTATIONS_R2_BUCKET": "maple-attestations-dev",
            "AWS_ACCESS_KEY_ID": "fixture-access-key", "AWS_SECRET_ACCESS_KEY": "fixture-secret-key",
            # Inherited credentials that must never reach a tool.
            "AWS_SESSION_TOKEN": "inherited-session-token", "AWS_PROFILE": "inherited-profile",
        }
        return [
            mock.patch.object(release, "ROOT", self.repo),
            mock.patch.dict(os.environ, environ, clear=True),
            mock.patch.object(release, "cosign_platform", return_value="linux-amd64"),
            mock.patch.dict(release.COSIGN_SHA256, {"linux-amd64": release.sha256_file(self.bin / "cosign")}),
            mock.patch.object(release, "github_oidc_token", return_value="header.claims.signature"),
            mock.patch.object(release, "READBACK_DELAY", 0),
            contextlib.redirect_stdout(io.StringIO()),
        ]

    def log(self, tool):
        path = self.state / f"{tool}.log"
        return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []

    # -- build evidence --------------------------------------------------------

    def built_artifact(self, files=None):
        """The files build_opensecret_eif.sh writes for this build. A build script failure fails the test."""
        files = files or self.files
        output = release_build_candidate(Path(tempfile.mkdtemp(dir=self.root)), mode="dev",
                                         source_sha=self.handoff["source_sha"], image=files["image.eif"],
                                         pcr=files["pcr.json"])
        return {name: (output / name).read_bytes() for name in (*release.FILES, "SHA256SUMS")}

    def forged_artifact(self, handoff=None, checksummed=release.FILES, files=None):
        """An artifact the build script never writes: this handoff, and SHA256SUMS over `checksummed`."""
        files = dict(files or self.files)
        files["handoff.json"] = json.dumps(handoff or self.handoff).encode()
        files["SHA256SUMS"] = "".join(f"{sha256_hex(files[name])}  {name}\n" for name in checksummed).encode()
        return files

    def stage(self, attested=None, shipped=None):
        """Write the gh fixture: the files the build attested (by default the build script's
        output) and the files its artifact now holds (by default the same)."""
        attested = attested or self.built_artifact()
        shipped = shipped or attested
        with zipfile.ZipFile(self.state / "artifact.zip", "w") as archive:
            for name, data in {**shipped, **self.extra_zip_entries}.items():
                archive.writestr(name, data)
        names = self.attested_names or [line.split("  ")[1] for line in attested["SHA256SUMS"].decode().splitlines()]
        invocation = f"{release.SOURCE_REPOSITORY}/actions/runs/{self.attested_run_id}/attempts/{self.attested_attempt}"
        statement = {
            "_type": release.STATEMENT_TYPE,
            "predicateType": release.SLSA_TYPE,
            "subject": [{"name": name, "digest": {"sha256": sha256_hex(attested[name])}} for name in names],
            "predicate": {"runDetails": {"metadata": {"invocationId": invocation}}},
        }
        (self.state / "gh.json").write_text(json.dumps({
            "run": self.run,
            "artifacts": [{"total_count": 1, "artifacts": [self.artifact]}],
            "attestations": [{"digests": [sha256_hex(attested[name]) for name in names], "statement": statement}],
        }))

    def prepare(self):
        if not (self.state / "gh.json").exists():
            self.stage()
        return release.prepare("dev", str(self.run_id), self.candidate, self.release)

    def statement(self):
        return (self.release / "statement.json").read_bytes()

    def signed_bundle(self, mutate=None):
        """The bundle the fake Cosign writes: valid for the prepared statement, or with one mutation."""
        bundle = make_bundle(self.statement())
        if mutate:
            mutate(bundle)
        (self.state / "signed-bundle.json").write_bytes(encode(bundle))

    def cosign_config(self, **values):
        (self.state / "cosign.json").write_text(json.dumps(values))


class PrepareTests(unittest.TestCase):
    def assert_refused(self, work, message=""):
        with self.assertRaisesRegex(release.EvidenceError, message):
            work.prepare()
        self.assertFalse((work.release / "statement.json").exists())

    def test_candidate_is_exactly_the_build_scripts_output(self):
        with workspace() as work:
            work.prepare()
            produced = release_build_candidate(Path(tempfile.mkdtemp(dir=work.root)), mode="dev",
                                               source_sha=work.source_sha, image=work.files["image.eif"],
                                               pcr=work.files["pcr.json"])
            for name in ("image.eif", "pcr.json", "handoff.json", "SHA256SUMS"):
                self.assertEqual((work.candidate / name).read_bytes(), (produced / name).read_bytes(), name)
            self.assertEqual(sorted(path.name for path in work.candidate.iterdir()),
                             ["SHA256SUMS", "handoff.json", "image.eif", "pcr.json"])
            self.assertFalse((work.release / "artifact.zip").exists())

    def test_verified_master_build_becomes_the_exact_statement(self):
        with workspace() as work:
            work.run["run_attempt"] = 2  # a later attempt re-ran only another job
            work.prepare()
            statement = json.loads(work.statement())
            self.assertEqual(statement["subject"],
                             [{"name": "image.eif", "digest": {"sha256": work.handoff["eif_sha256"]}}])
            self.assertEqual(statement["predicate"], {
                "environment": "development",
                "source": {"repository": "https://github.com/MaplePrivacyLabs/Maple", "commit": work.source_sha},
                "measurements": {"PCR0": "a" * 96, "PCR1": "b" * 96, "PCR2": "c" * 96},
                "build": {"runId": str(work.run_id), "runAttempt": 1},
            })

    def test_step_outputs_name_the_verified_build(self):
        with workspace() as work:
            work.prepare()
            outputs = dict(line.split("=", 1) for line in (work.root / "github-output").read_text().splitlines())
            self.assertEqual(outputs, {"source_sha": work.source_sha, "eif_sha256": work.handoff["eif_sha256"],
                                       "pcr0": "a" * 96})

    def test_every_file_is_checked_against_the_release_workflows_provenance(self):
        with workspace() as work:
            work.prepare()
            verify = [args for args in work.log("gh") if args[:2] == ["attestation", "verify"]]
            self.assertEqual(sorted(Path(args[2]).name for args in verify), ["handoff.json", "image.eif", "pcr.json"])
            for args in verify:
                options = dict(zip(args[3::2], args[4::2]))
                self.assertEqual(options, {
                    "--repo": "MaplePrivacyLabs/Maple",
                    "--cert-identity": "https://github.com/MaplePrivacyLabs/Maple/.github/workflows/"
                                       "opensecret-eif-release.yml@refs/heads/master",
                    "--cert-oidc-issuer": "https://token.actions.githubusercontent.com",
                    "--source-ref": "refs/heads/master",
                    "--source-digest": work.source_sha,
                    "--signer-digest": work.source_sha,
                    "--predicate-type": "https://slsa.dev/provenance/v1",
                    "--format": "json",
                })

    def test_runs_other_than_a_successful_master_dispatch_of_the_release_workflow_are_refused(self):
        wrong = {
            "path": ".github/workflows/opensecret-eif.yml",
            "event": "push",
            "head_branch": "feature",
            "conclusion": "failure",
            "status": "in_progress",
            "head_sha": "abc",
            "head_repository": {"full_name": "fork/Maple"},
            "repository": {"full_name": "fork/Maple", "private": False},
        }
        for field, value in wrong.items():
            with self.subTest(field=field), workspace() as work:
                work.run[field] = value
                self.assert_refused(work)
                self.assertFalse(work.candidate.exists())

    def test_build_commit_outside_master_history_is_refused(self):
        with workspace() as work:
            git("checkout", "-q", "-b", "side", work.source_sha, cwd=work.repo)
            (work.repo / "side").write_text("unreviewed\n")
            git("add", "-A", cwd=work.repo)
            git("commit", "-q", "-m", "side", cwd=work.repo)
            side = git("rev-parse", "HEAD", cwd=work.repo)
            git("checkout", "-q", "master", cwd=work.repo)
            work.run["head_sha"] = side
            work.handoff["source_sha"] = side
            work.artifact["workflow_run"]["head_sha"] = side
            work.artifact["name"] = f"opensecret-eif-dev-{side[:12]}"
            self.assert_refused(work, "not in this master checkout")

    def test_artifact_must_be_the_runs_own_unexpired_dev_artifact(self):
        cases = {
            "another run": lambda artifact: artifact["workflow_run"].update(id=1),
            "another commit": lambda artifact: artifact["workflow_run"].update(head_sha="2" * 40),
            "expired": lambda artifact: artifact.update(expired=True),
            "prod artifact name": lambda artifact: artifact.update(name=artifact["name"].replace("-dev-", "-prod-")),
        }
        for name, change in cases.items():
            with self.subTest(case=name), workspace() as work:
                change(work.artifact)
                self.assert_refused(work)

    def test_artifact_files_changed_after_the_build_are_refused(self):
        with workspace() as work:
            shipped = work.built_artifact()
            shipped["image.eif"] = b"substituted EIF"
            work.stage(shipped=shipped)
            self.assert_refused(work, "differ from the build's SHA256SUMS")

    def test_substituted_eif_with_matching_checksums_lacks_this_runs_provenance(self):
        with workspace() as work:
            work.stage(shipped=work.forged_artifact(files={**work.files, "image.eif": b"substituted"}))
            self.assert_refused(work, "provenance for image.eif")

    def test_extra_artifact_files_are_refused(self):
        with workspace() as work:
            work.extra_zip_entries = {"extra.sh": b"echo"}
            work.stage()
            self.assert_refused(work, "exactly image.eif")

    def test_builds_made_before_the_handoff_was_attested_are_refused(self):
        with workspace() as work:
            work.stage(attested=work.forged_artifact(checksummed=("image.eif", "pcr.json")))
            self.assert_refused(work, "predates attested handoffs")

    def test_provenance_must_cover_all_three_files(self):
        with workspace() as work:
            work.attested_names = ["image.eif", "pcr.json"]
            work.stage()
            self.assert_refused(work, "does not cover")

    def test_provenance_from_another_run_of_the_release_workflow_is_refused(self):
        with workspace() as work:
            work.attested_run_id = 5000
            work.stage()
            self.assert_refused(work, "does not cover")

    def test_provenance_from_an_attempt_the_run_never_reached_is_refused(self):
        with workspace() as work:
            work.attested_attempt = 2
            work.stage()
            self.assert_refused(work, "does not cover")

    def test_handoff_must_say_dev_and_agree_with_the_artifact(self):
        cases = {
            "prod build": {"environment": "prod"},
            "other commit": {"source_sha": "3" * 40},
            "other EIF": {"eif_sha256": "0" * 64},
            "other measurements": {"measurements": {**VALID_MEASUREMENTS, "PCR1": "e" * 96}},
            "extra field": {"environment": "dev", "signed_environment": "production"},
        }
        for name, change in cases.items():
            with self.subTest(case=name), workspace() as work:
                work.stage(attested=work.forged_artifact(handoff={**work.handoff, **change}))
                self.assert_refused(work)

    def test_malformed_measurements_are_refused(self):
        with workspace() as work:
            # The build script refuses an all-zero PCR, so only a forged artifact carries one.
            work.files["pcr.json"] = json.dumps({**VALID_MEASUREMENTS, "PCR0": "0" * 96}).encode()
            work.handoff["measurements"] = json.loads(work.files["pcr.json"])
            work.stage(attested=work.forged_artifact())
            self.assert_refused(work, "Invalid PCR0")


class BuildEvidenceShapeTests(unittest.TestCase):
    """GitHub responses whose parts are not the expected JSON types fail with an evidence error."""

    def setUp(self):
        self.run = {"id": 4242, "run_attempt": 1, "head_sha": "1" * 40}
        self.artifact = {"id": 99, "name": f"opensecret-eif-dev-{'1' * 12}", "expired": False, "size_in_bytes": 4096,
                         "workflow_run": {"id": 4242, "head_sha": "1" * 40}}

    def test_the_well_formed_artifact_listing_is_accepted(self):
        self.assertEqual(release.select_artifact([{"artifacts": [self.artifact]}], self.run, "dev"), self.artifact)

    def test_pages_with_no_or_empty_artifacts_list_nothing(self):
        for empty in ({}, {"artifacts": []}, {"artifacts": ""}, {"artifacts": {}}):
            with self.subTest(page=empty):
                pages = [empty, {"artifacts": [self.artifact]}]
                self.assertEqual(release.select_artifact(pages, self.run, "dev"), self.artifact)

    def test_malformed_artifact_listings_are_refused(self):
        cases = {
            "page not an object": ([True], "page is not a JSON object"),
            "artifacts not a list": ([{"artifacts": "opensecret"}], "has no artifact list"),
            "null artifacts": ([{"artifacts": None}], "has no artifact list"),
            "artifact not an object": ([{"artifacts": [True, self.artifact]}], "Listed artifact is not a JSON object"),
            "workflow run not an object": ([{"artifacts": [{**self.artifact, "workflow_run": [4242]}]}],
                                           "workflow run is not a JSON object"),
            "size not a number": ([{"artifacts": [{**self.artifact, "size_in_bytes": "4096"}]}],
                                  "size is out of bounds"),
        }
        for name, (pages, message) in cases.items():
            with self.subTest(case=name), self.assertRaisesRegex(release.EvidenceError, message):
                release.select_artifact(pages, self.run, "dev")

    def test_malformed_build_provenance_is_refused(self):
        statement = {"_type": release.STATEMENT_TYPE, "predicateType": release.SLSA_TYPE}
        cases = {
            "result not an object": [True],
            "verification result not an object": [{"verificationResult": [1]}],
            "statement not an object": [{"verificationResult": {"statement": [1]}}],
            "predicate not an object": [{"verificationResult": {"statement": {**statement, "predicate": [1]}}}],
            "run details not an object": [{"verificationResult": {"statement": {
                **statement, "predicate": {"runDetails": "x"}}}}],
            "metadata not an object": [{"verificationResult": {"statement": {
                **statement, "predicate": {"runDetails": {"metadata": [1]}}}}}],
        }
        for name, results in cases.items():
            with self.subTest(case=name), self.assertRaisesRegex(release.EvidenceError, "is not a JSON object"):
                release.provenance_attempts(results, self.run, {})

    def provenance(self, subject):
        """A verified result for attempt 1 of this run whose statement lists `subject`."""
        invocation = f"{release.SOURCE_REPOSITORY}/actions/runs/4242/attempts/1"
        return {"verificationResult": {"statement": {
            "_type": release.STATEMENT_TYPE, "predicateType": release.SLSA_TYPE, "subject": subject,
            "predicate": {"runDetails": {"metadata": {"invocationId": invocation}}}}}}

    def test_malformed_provenance_subjects_are_refused(self):
        digest = {"sha256": "d" * 64}
        cases = {
            "subjects true": True,
            "subjects a number": 1,
            "name a list": [{"name": [], "digest": digest}],
            "name a nonempty list": [{"name": ["image.eif"], "digest": digest}],
            "name an object": [{"name": {}, "digest": digest}],
            "name a nonempty object": [{"name": {"x": 1}, "digest": digest}],
            "digest a list": [{"name": "image.eif", "digest": {"sha256": []}}],
            "digest a nonempty list": [{"name": "image.eif", "digest": {"sha256": ["d" * 64]}}],
            "digest an object": [{"name": "image.eif", "digest": {"sha256": {}}}],
            "digest a nonempty object": [{"name": "image.eif", "digest": {"sha256": {"x": 1}}}],
        }
        valid = self.provenance([{"name": "image.eif", "digest": digest}])
        for name, subject in cases.items():
            with self.subTest(case=name), self.assertRaisesRegex(release.EvidenceError, "Build provenance subject"):
                release.provenance_attempts([self.provenance(subject), valid], self.run, {"image.eif": "d" * 64})

    def test_subjects_that_cover_nothing_are_skipped_as_before(self):
        digest = {"sha256": "d" * 64}
        cases = {
            "no subjects": None, "empty list": [], "string": "image.eif", "object": {"image.eif": digest},
            "string entries": ["image.eif"], "entry without digest": [{"name": "image.eif"}],
            "digest not an object": [{"name": "image.eif", "digest": ["d" * 64]}],
            "list name without digest": [{"name": ["image.eif"]}],
            "numeric name": [{"name": 1, "digest": digest}], "null digest": [{"name": "image.eif", "digest": {}}],
        }
        valid = self.provenance([{"name": "image.eif", "digest": digest}])
        for name, subject in cases.items():
            with self.subTest(case=name):
                attempts = release.provenance_attempts([self.provenance(subject), valid], self.run,
                                                       {"image.eif": "d" * 64})
                self.assertEqual(attempts, {1})

    def test_malformed_build_run_repositories_are_refused(self):
        for field in ("repository", "head_repository"):
            run = {"id": 4242, "repository": {"full_name": release.REPOSITORY, "private": False},
                   "head_repository": {"full_name": release.REPOSITORY}, field: [release.REPOSITORY]}
            with self.subTest(field=field), self.assertRaisesRegex(release.EvidenceError, "is not a JSON object"):
                release.validate_run(run, "4242")


def foreign_repository(root, keys):
    """Another repository whose HEAD tracks `keys` (path -> DER), to point inherited Git variables at."""
    foreign = root / "foreign"
    foreign.mkdir()
    git("init", "-q", "--initial-branch=master", cwd=foreign)
    for configured, der in keys.items():
        (foreign / configured).parent.mkdir(parents=True, exist_ok=True)
        (foreign / configured).write_text(pem(der))
    git("add", "-A", cwd=foreign)
    git("commit", "-q", "-m", "foreign keys", cwd=foreign)
    return foreign


def git_selectors(foreign):
    """Inherited Git variables that would make git read `foreign` instead of the checkout."""
    return {
        "GIT_DIR": {"GIT_DIR": str(foreign / ".git")},
        "GIT_DIR and GIT_WORK_TREE": {"GIT_DIR": str(foreign / ".git"), "GIT_WORK_TREE": str(foreign)},
        "GIT_COMMON_DIR": {"GIT_COMMON_DIR": str(foreign / ".git")},
    }


class TokenEndpoint(http.server.BaseHTTPRequestHandler):
    """GitHub's OIDC token endpoint, answering every request with `body`."""

    body: bytes = b""

    def do_GET(self):  # noqa: N802 - http.server API
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(self.body)))
        self.end_headers()
        self.wfile.write(self.body)

    def log_message(self, *args):
        pass


class GitHubOidcTokenTests(unittest.TestCase):
    def setUp(self):
        self.handler = type("FixtureTokenEndpoint", (TokenEndpoint,), {})
        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), self.handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        self.addCleanup(thread.join)
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        url = f"http://127.0.0.1:{server.server_address[1]}/token"
        self.enterContext(mock.patch.dict(os.environ, {"ACTIONS_ID_TOKEN_REQUEST_URL": url,
                                                       "ACTIONS_ID_TOKEN_REQUEST_TOKEN": "fixture-request-token"}))

    def token_for(self, body):
        self.handler.body = body
        return release.github_oidc_token()

    def test_the_token_in_githubs_response_is_returned(self):
        self.assertEqual(self.token_for(b'{"count": 1, "value": "header.claims.signature"}'), "header.claims.signature")

    def test_a_response_without_a_token_is_refused(self):
        for body in (b"{}", b'{"value": "not-a-jwt"}', b'{"value": 7}', b"[]", b"null", b"false", b'""', b"0"):
            with self.subTest(body=body):
                with self.assertRaisesRegex(release.EvidenceError, "GitHub returned no OIDC token"):
                    self.token_for(body)

    def test_a_response_that_is_not_an_object_is_refused(self):
        for body in (b"[1]", b'["header.claims.signature"]', b'"header.claims.signature"', b"7", b"true"):
            with self.subTest(body=body):
                with self.assertRaisesRegex(release.EvidenceError, "OIDC response is not a JSON object"):
                    self.token_for(body)


class ReviewedPublicKeyTests(unittest.TestCase):
    """Cosign verifies against exactly the key file the checked-out commit tracks."""

    def setUp(self):
        self.work = self.enterContext(workspace())
        self.key = self.work.repo / DEV_PUBLIC_KEY

    def assert_refused(self, configured, message):
        with self.assertRaisesRegex(release.EvidenceError, message):
            release.reviewed_public_key({"SIGNING_PUBLIC_KEY": configured})

    def test_the_tracked_unmodified_key_is_used(self):
        self.assertEqual(release.reviewed_public_key({"SIGNING_PUBLIC_KEY": DEV_PUBLIC_KEY}),
                         (self.key, REVIEWED_DER))

    def test_a_key_the_commit_does_not_track_is_refused(self):
        (self.work.repo / UNTRACKED_PUBLIC_KEY).write_text(pem(REVIEWED_DER))
        with self.subTest(case="untracked"):
            self.assert_refused(UNTRACKED_PUBLIC_KEY, "is not committed")
        git("add", UNTRACKED_PUBLIC_KEY, cwd=self.work.repo)
        with self.subTest(case="staged but not committed"):
            self.assert_refused(UNTRACKED_PUBLIC_KEY, "is not committed")

    def test_a_file_where_the_commit_has_a_directory_is_refused(self):
        (self.work.repo / "keys").mkdir()
        (self.work.repo / "keys/inner.pem").write_text(pem(REVIEWED_DER))
        self.work.commit("a directory of keys")
        (self.work.repo / "keys/inner.pem").unlink()
        (self.work.repo / "keys").rmdir()
        (self.work.repo / "keys").write_text(pem(REVIEWED_DER))
        self.assert_refused("keys", "is not committed")

    def test_a_key_changed_after_checkout_is_refused(self):
        self.key.write_text(pem(OTHER_DER))
        with self.subTest(case="edited"):
            self.assert_refused(DEV_PUBLIC_KEY, "differs from its committed content")
        git("add", DEV_PUBLIC_KEY, cwd=self.work.repo)
        with self.subTest(case="edited and staged"):
            self.assert_refused(DEV_PUBLIC_KEY, "differs from its committed content")

    def test_a_symlink_to_the_tracked_key_is_refused(self):
        (self.work.repo / LINKED_PUBLIC_KEY).symlink_to(self.key.name)
        with self.subTest(case="untracked link"):
            self.assert_refused(LINKED_PUBLIC_KEY, "reached through a symlink")
        self.work.commit("a link to the reviewed key")
        with self.subTest(case="tracked link"):
            self.assert_refused(LINKED_PUBLIC_KEY, "reached through a symlink")

    def test_a_key_under_a_symlinked_directory_is_refused(self):
        (self.work.repo / "keys").symlink_to(Path(DEV_PUBLIC_KEY).parent)
        self.work.commit("a linked key directory")
        self.assert_refused(f"keys/{self.key.name}", "reached through a symlink")

    def test_inherited_git_variables_cannot_make_another_repository_vouch_for_the_key(self):
        foreign = foreign_repository(self.work.root, {DEV_PUBLIC_KEY: OTHER_DER, UNTRACKED_PUBLIC_KEY: OTHER_DER})
        (self.work.repo / UNTRACKED_PUBLIC_KEY).write_text(pem(OTHER_DER))
        for name, selectors in git_selectors(foreign).items():
            with self.subTest(selectors=name), mock.patch.dict(os.environ, selectors):
                # The checkout's own tracked key is still the one read.
                self.assertEqual(release.reviewed_public_key({"SIGNING_PUBLIC_KEY": DEV_PUBLIC_KEY}),
                                 (self.key, REVIEWED_DER))
                self.assert_refused(UNTRACKED_PUBLIC_KEY, "is not committed")
                self.key.write_text(pem(OTHER_DER))
                self.assert_refused(DEV_PUBLIC_KEY, "differs from its committed content")
                self.key.write_text(pem(REVIEWED_DER))

    def test_a_linked_worktree_reads_its_own_head(self):
        linked = self.work.root / "linked"
        git("worktree", "add", "-q", "--detach", str(linked), "HEAD", cwd=self.work.repo)
        with mock.patch.object(release, "ROOT", linked):
            self.assertEqual(release.reviewed_public_key({"SIGNING_PUBLIC_KEY": DEV_PUBLIC_KEY}),
                             (linked / DEV_PUBLIC_KEY, REVIEWED_DER))
            (linked / DEV_PUBLIC_KEY).write_text(pem(OTHER_DER))
            self.assert_refused(DEV_PUBLIC_KEY, "differs from its committed content")

    def test_a_directory_that_is_not_its_own_checkout_is_refused(self):
        nested = self.work.repo / "nested"
        (nested / DEV_PUBLIC_KEY).parent.mkdir(parents=True)
        (nested / DEV_PUBLIC_KEY).write_text(pem(REVIEWED_DER))
        self.work.commit("a key below a subdirectory")
        plain = self.work.root / "plain"
        (plain / DEV_PUBLIC_KEY).parent.mkdir(parents=True)
        (plain / DEV_PUBLIC_KEY).write_text(pem(REVIEWED_DER))
        for name, root in (("subdirectory of a checkout", nested), ("no repository", plain)):
            with self.subTest(case=name), mock.patch.object(release, "ROOT", root):
                self.assert_refused(DEV_PUBLIC_KEY, "not a Git checkout of its own")

    def test_a_checkout_whose_head_cannot_be_read_is_refused(self):
        unborn = self.work.root / "unborn"
        unborn.mkdir()
        git("init", "-q", cwd=unborn)
        (unborn / DEV_PUBLIC_KEY).parent.mkdir(parents=True)
        (unborn / DEV_PUBLIC_KEY).write_text(pem(REVIEWED_DER))
        with mock.patch.object(release, "ROOT", unborn):
            self.assert_refused(DEV_PUBLIC_KEY, "Cannot read the checked-out commit's tree")

    def test_a_key_whose_committed_blob_is_missing_is_refused(self):
        blob = git("rev-parse", f"HEAD:{DEV_PUBLIC_KEY}", cwd=self.work.repo)
        (self.work.repo / ".git/objects" / blob[:2] / blob[2:]).unlink()
        self.assert_refused(DEV_PUBLIC_KEY, "Cannot read the checked-out commit's tree")

    def test_missing_git_is_refused(self):
        empty = self.work.root / "no-tools"
        empty.mkdir()
        with mock.patch.dict(os.environ, {"PATH": str(empty)}):
            self.assert_refused(DEV_PUBLIC_KEY, "Git is not available")


# Bundles that sign must refuse, so that publish never uploads them.
UNPUBLISHABLE = ("no inclusion proof", "no timestamp", "Rekor v1 entry", "extra v1 entry", "inclusion promise",
                 "zero integrated time string", "zero integrated time number", "tampered statement",
                 "other signer key")


class SignTests(unittest.TestCase):
    def setUp(self):
        self.work = self.enterContext(workspace())
        self.work.prepare()
        self.cosign = self.work.bin / "cosign"

    def sign(self):
        return release.sign("dev", self.work.candidate, self.work.release, self.cosign)

    def assert_never_signed(self, message=""):
        with self.assertRaisesRegex(release.EvidenceError, message):
            self.sign()
        self.assertNotIn("attest-blob", [call["args"][0] for call in self.work.log("cosign")])
        self.assertFalse((self.work.release / "bundle.json").exists())

    def test_signs_the_prepared_statement_with_the_fixed_key_and_pinned_configuration(self):
        self.work.signed_bundle()
        self.sign()
        calls = self.work.log("cosign")
        self.assertEqual([call["args"][0] for call in calls], ["public-key", "attest-blob", "verify-blob-attestation"])
        self.assertEqual(calls[0]["args"], ["public-key", "--key", f"awskms:///{KEY_ARN}"])
        self.assertEqual(calls[1]["args"], [
            "attest-blob", "--key", f"awskms:///{KEY_ARN}", "--statement", str(self.work.release / "statement.json"),
            "--signing-config", str(release.SIGNING_CONFIG), "--trusted-root", str(release.TRUSTED_ROOT),
            "--bundle", str(self.work.release / "bundle.json"), "--yes", str(self.work.candidate / "image.eif")])
        self.assertEqual(calls[2]["args"], [
            "verify-blob-attestation", "--bundle", str(self.work.release / "bundle.json"),
            "--key", str(self.work.repo / DEV_PUBLIC_KEY),
            "--type", release.PREDICATE_TYPE, "--use-signed-timestamps", "--trusted-root", str(release.TRUSTED_ROOT),
            str(self.work.candidate / "image.eif")])

    def test_only_the_role_assumed_through_github_oidc_reaches_kms(self):
        self.work.signed_bundle()
        self.sign()
        for call in self.work.log("cosign")[:2]:  # public-key and attest-blob
            self.assertEqual(call["env"]["AWS_ROLE_ARN"], ROLE_ARN)
            self.assertEqual(call["env"]["AWS_REGION"], "us-east-1")
            self.assertEqual(call["token"], "header.claims.signature")
            self.assertFalse({"AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY", "AWS_SESSION_TOKEN", "AWS_PROFILE"}
                             & set(call["env"]))
            self.assertFalse(Path(call["env"]["AWS_WEB_IDENTITY_TOKEN_FILE"]).exists())  # removed afterwards

    def test_records_the_cosign_release_and_key_that_signed(self):
        self.work.signed_bundle()
        self.sign()
        receipt = json.loads((self.work.release / "bundle.verification.json").read_text())
        self.assertEqual(receipt["cosign"], "v3.1.4")
        self.assertEqual(receipt["keyArn"], KEY_ARN)

    def test_placeholder_or_unfixed_identity_never_reaches_cosign(self):
        cases = {
            "placeholder key": {"SIGNING_KEY_ARN": "arn:aws:kms:us-east-1:REPLACE_ME_SIGNING_ACCOUNT_ID:key/"
                                                  "REPLACE_ME_DEV_KEY_ID"},
            "placeholder role": {"SIGNER_ROLE_ARN": "arn:aws:iam::REPLACE_ME_SIGNING_ACCOUNT_ID:role/"
                                                    "maple-pcr-signer-dev"},
            "alias": {"SIGNING_KEY_ARN": f"arn:aws:kms:us-east-1:{ACCOUNT}:alias/maple-dev"},
            "bare alias": {"SIGNING_KEY_ARN": "alias/maple-dev"},
            "other region": {"SIGNING_KEY_ARN": KEY_ARN.replace("us-east-1", "eu-west-1")},
            "prod role": {"SIGNER_ROLE_ARN": ROLE_ARN.replace("-dev", "-prod")},
            "other account": {"SIGNER_ROLE_ARN": ROLE_ARN.replace(ACCOUNT, "210987654321")},
            "missing key": {"SIGNING_KEY_ARN": ""},
            "uncommitted public key": {"SIGNING_PUBLIC_KEY": "scripts/ci/opensecret-release/missing.pem"},
            "public key outside the repository": {"SIGNING_PUBLIC_KEY": "../outside.pem"},
        }
        # Until the workflow's own identity is complete (no placeholder, public key
        # committed), it must be refused like the cases above.
        job_env = workflows()["opensecret-release-statement-dev.yml"]["jobs"]["sign"]["env"]
        configured = {name: job_env[name] for name in ("SIGNING_KEY_ARN", "SIGNER_ROLE_ARN", "SIGNING_PUBLIC_KEY")}
        has_placeholder = any("REPLACE" in value for value in configured.values())
        if has_placeholder or not (ROOT / configured["SIGNING_PUBLIC_KEY"]).is_file():
            cases["the workflow's configured identity"] = configured
        (self.work.root / "outside.pem").write_text(pem(REVIEWED_DER))
        self.work.signed_bundle()
        for name, values in cases.items():
            with self.subTest(case=name), mock.patch.dict(os.environ, values):
                self.assert_never_signed()
                self.assertEqual(self.work.log("cosign"), [])
                with contextlib.redirect_stderr(io.StringIO()):
                    self.assertEqual(release.main(["preflight", "dev"]), 1)

    def assert_refused_before_any_credential(self):
        self.assert_never_signed("refusing to sign")
        self.assertEqual(self.work.log("cosign"), [])
        release.github_oidc_token.assert_not_called()
        with contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(release.main(["preflight", "dev"]), 1)

    def test_an_untracked_or_symlinked_public_key_is_refused_before_any_credential(self):
        (self.work.repo / UNTRACKED_PUBLIC_KEY).write_text(pem(REVIEWED_DER))
        (self.work.repo / LINKED_PUBLIC_KEY).symlink_to(Path(DEV_PUBLIC_KEY).name)
        self.work.signed_bundle()
        for configured in (UNTRACKED_PUBLIC_KEY, LINKED_PUBLIC_KEY):
            with self.subTest(key=configured), mock.patch.dict(os.environ, {"SIGNING_PUBLIC_KEY": configured}):
                self.assert_refused_before_any_credential()

    def test_a_foreign_repository_cannot_vouch_for_an_edited_key_before_any_credential(self):
        foreign = foreign_repository(self.work.root, {DEV_PUBLIC_KEY: OTHER_DER})
        (self.work.repo / DEV_PUBLIC_KEY).write_text(pem(OTHER_DER))
        self.work.signed_bundle()
        for name, selectors in git_selectors(foreign).items():
            with self.subTest(selectors=name), mock.patch.dict(os.environ, selectors):
                self.assert_refused_before_any_credential()

    def test_a_public_key_edited_after_checkout_is_refused_before_any_credential(self):
        (self.work.repo / DEV_PUBLIC_KEY).write_text(pem(OTHER_DER))
        self.work.signed_bundle()
        self.assert_refused_before_any_credential()

    def test_kms_key_other_than_the_reviewed_key_is_refused_before_signing(self):
        (self.work.state / "served.pem").write_text(pem(OTHER_DER))
        self.work.signed_bundle()
        self.assert_never_signed("differs from the reviewed public key")

    def test_tampered_statement_is_refused_before_signing(self):
        tampered = self.work.statement().replace(b'"development"', b'"production"')
        (self.work.release / "statement.json").write_bytes(tampered)
        self.work.signed_bundle()
        self.assert_never_signed("statement was modified")

    def test_eif_changed_after_verification_is_refused_before_signing(self):
        self.work.signed_bundle()
        (self.work.candidate / "image.eif").write_bytes(b"changed after verification")
        self.assert_never_signed("SHA256SUMS")

    def test_an_unpinned_cosign_binary_is_refused(self):
        self.work.signed_bundle()
        with mock.patch.dict(release.COSIGN_SHA256, {"linux-amd64": "0" * 64}):
            self.assert_never_signed("not the pinned")

    def test_missing_proof_timestamp_or_extra_entries_fail_and_nothing_is_published(self):
        for name in UNPUBLISHABLE:
            with self.subTest(mutation=name):
                bundle = self.work.release / "bundle.json"
                bundle.unlink(missing_ok=True)
                self.work.signed_bundle(MUTATIONS[name])
                with self.assertRaises(release.EvidenceError):
                    self.sign()
                self.assertFalse((self.work.release / "bundle.verification.json").exists())
                with self.assertRaises(release.EvidenceError):
                    release.publish("dev", self.work.candidate, self.work.release, self.cosign,
                                    base_url="http://127.0.0.1:9")
                self.assertEqual(self.work.log("aws"), [])

    def test_failed_attestation_fails_the_job_without_a_bundle(self):
        self.work.signed_bundle()
        self.work.cosign_config(attest_fails=True)
        with self.assertRaises(subprocess.CalledProcessError):
            self.sign()
        self.assertFalse((self.work.release / "bundle.json").exists())

    def test_failed_cosign_verification_fails_the_job_without_a_receipt(self):
        self.work.signed_bundle()
        self.work.cosign_config(verify_fails=True)
        with self.assertRaises(subprocess.CalledProcessError):
            self.sign()
        self.assertFalse((self.work.release / "bundle.verification.json").exists())


class Host(http.server.BaseHTTPRequestHandler):
    """The public bucket host: serves stored objects with their stored metadata.

    `overrides` replaces the body, the cache policy or the CORS header, modeling a
    stale cache or a misconfigured host; `origins` records each request's Origin.
    """

    storage: Path
    overrides: dict = {}
    origins: list

    def do_GET(self):  # noqa: N802 - http.server API
        self.origins.append(self.headers.get("Origin"))
        path = self.storage / "maple-attestations-dev" / self.path.lstrip("/")
        if not path.is_file():
            self.send_response(404)
            self.end_headers()
            return
        meta = json.loads(Path(str(path) + ".meta").read_text())
        body = self.overrides.get("body", path.read_bytes())
        self.send_response(200)
        self.send_header("Content-Type", meta["type"])
        self.send_header("Cache-Control", self.overrides.get("cache", meta["cache"]))
        if self.overrides.get("cors", True):
            self.send_header("Access-Control-Allow-Origin", "*")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


class PublishTests(unittest.TestCase):
    def setUp(self):
        self.work = self.enterContext(workspace())
        self.work.prepare()
        self.work.signed_bundle()
        self.cosign = self.work.bin / "cosign"
        release.sign("dev", self.work.candidate, self.work.release, self.cosign)
        self.handler = type("FixtureHost", (Host,), {"storage": self.work.storage, "overrides": {}, "origins": []})
        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), self.handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        self.addCleanup(thread.join)
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        self.base_url = f"http://127.0.0.1:{server.server_address[1]}"
        self.key = f"{'a' * 96}.json"
        self.bucket = self.work.storage / "maple-attestations-dev"

    def publish(self):
        return release.publish("dev", self.work.candidate, self.work.release, self.cosign, base_url=self.base_url)

    def test_uploads_one_object_and_confirms_the_public_bytes(self):
        self.publish()
        bundle = (self.work.release / "bundle.json").read_bytes()
        self.assertEqual((self.bucket / self.key).read_bytes(), bundle)
        self.assertEqual(json.loads((self.bucket / f"{self.key}.meta").read_text()),
                         {"type": "application/json", "cache": "public, max-age=300"})
        upload = self.work.log("aws")
        self.assertEqual(len(upload), 1)
        options = dict(zip(upload[0]["args"][2::2], upload[0]["args"][3::2]))
        self.assertEqual(options["--endpoint-url"], f"https://{R2_ACCOUNT}.r2.cloudflarestorage.com")
        self.assertEqual((options["--bucket"], options["--key"]), ("maple-attestations-dev", self.key))
        # Only the bucket credential reaches the upload; no inherited session or profile.
        self.assertIn("AWS_ACCESS_KEY_ID", upload[0]["env"])
        self.assertFalse({"AWS_SESSION_TOKEN", "AWS_PROFILE", "AWS_ROLE_ARN"} & set(upload[0]["env"]))
        self.assertEqual(self.handler.origins, [release.READBACK_ORIGIN])
        self.assertEqual((self.work.release / "public.bundle.json").read_bytes(), bundle)
        receipt = json.loads((self.work.release / "publication.json").read_text())
        self.assertEqual(receipt["url"], f"{self.base_url}/{self.key}")
        self.assertEqual(receipt["bundleSha256"], hashlib.sha256(bundle).hexdigest())
        self.assertEqual(receipt["environment"], "development")
        verified = [call["args"][2] for call in self.work.log("cosign") if call["args"][0] == "verify-blob-attestation"]
        self.assertIn(str(self.work.release / "public.bundle.json"), verified)

    def test_identical_rerun_and_same_pcr_replacement_keep_other_objects(self):
        self.bucket.mkdir(parents=True)
        other = self.bucket / f"{'9' * 96}.json"
        other.write_bytes(b"historical approval")
        (self.bucket / self.key).write_bytes(b"earlier approval for the same PCRs")
        Path(str(self.bucket / self.key) + ".meta").write_text(json.dumps({"type": "application/json",
                                                                          "cache": "public, max-age=300"}))
        self.publish()
        self.publish()  # an identical rerun
        self.assertEqual((self.bucket / self.key).read_bytes(), (self.work.release / "bundle.json").read_bytes())
        self.assertEqual(other.read_bytes(), b"historical approval")
        self.assertEqual(sorted(path.name for path in self.bucket.iterdir() if not path.name.endswith(".meta")),
                         sorted([other.name, self.key]))

    def assert_not_published(self, message):
        with self.assertRaisesRegex(release.EvidenceError, message):
            self.publish()
        self.assertFalse((self.work.release / "publication.json").exists())
        self.assertNotIn("Published", (self.work.root / "summary.md").read_text())

    def test_failed_upload_is_not_reported_as_published(self):
        (self.work.state / "aws-fails").touch()
        self.assert_not_published("Upload of .* failed")
        self.assertFalse((self.bucket / self.key).exists())

    def test_stale_or_unbrowsable_public_bytes_are_not_reported_as_published(self):
        cases = {
            "stale bytes": {"body": b"{}"},
            "wrong cache policy": {"cache": "public, max-age=31536000, immutable"},
            "no CORS": {"cors": False},
        }
        for name, override in cases.items():
            with self.subTest(case=name):
                self.handler.overrides = override
                self.assert_not_published("was not confirmed")
        self.assertEqual(len(self.handler.origins), len(cases) * release.READBACK_ATTEMPTS)

    def test_publisher_is_bound_to_the_dev_bucket_and_needs_its_credential(self):
        cases = {
            "prod bucket": {"ATTESTATIONS_R2_BUCKET": "maple-attestations-prod"},
            "bad account": {"ATTESTATIONS_R2_ACCOUNT_ID": "not-an-account"},
            "no credential": {"AWS_SECRET_ACCESS_KEY": ""},
        }
        for name, values in cases.items():
            with self.subTest(case=name), mock.patch.dict(os.environ, values):
                self.assert_not_published("")
        self.assertEqual(self.work.log("aws"), [])

    def test_a_public_key_edited_after_signing_is_never_uploaded(self):
        (self.work.repo / DEV_PUBLIC_KEY).write_text(pem(OTHER_DER))
        self.assert_not_published("differs from its committed content")
        self.assertEqual(self.work.log("aws"), [])

    def test_a_bundle_changed_after_signing_is_never_uploaded(self):
        bundle = json.loads((self.work.release / "bundle.json").read_text())
        tamper_statement(bundle)
        (self.work.release / "bundle.json").write_bytes(encode(bundle))
        self.assert_not_published("differs from the prepared statement")
        self.assertEqual(self.work.log("aws"), [])


if __name__ == "__main__":
    unittest.main()
