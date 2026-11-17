"""Behaviour of the OpenSecret release statement producer (opensecret_release_statement.py).

GitHub, Cosign, KMS and R2 are replaced by local fakes; a real Cosign-produced
bundle checks the evidence profile against actual Rekor v2 output.
"""

from __future__ import annotations

import base64
import contextlib
import hashlib
import http.server
import io
import itertools
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
from test_workflow_policy import workflows

ROOT = Path(__file__).resolve().parents[2]
FIXTURE = ROOT / "scripts/ci/fixtures/opensecret-release/cosign-v3.1.4-rekor-v2.bundle.json"
MEASUREMENTS = {"HashAlgorithm": "Sha384 { ... }", "PCR0": "a" * 96, "PCR1": "b" * 96, "PCR2": "c" * 96}
REVIEWED_DER = release.P384_SPKI_PREFIX + b"\x04" + bytes(range(1, 97))
OTHER_DER = release.P384_SPKI_PREFIX + b"\x04" + bytes(range(2, 98))
ACCOUNT = "123456789012"
KEY_ARN = f"arn:aws:kms:us-east-1:{ACCOUNT}:key/0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0"
ROLE_ARN = f"arn:aws:iam::{ACCOUNT}:role/maple-pcr-signer-dev"
R2_ACCOUNT = "e" * 32


def pem(der):
    body = base64.encodebytes(der).decode()
    return f"-----BEGIN PUBLIC KEY-----\n{body}-----END PUBLIC KEY-----\n"


def git(*arguments, cwd):
    return subprocess.run(["git", "-c", "user.name=fixture", "-c", "user.email=fixture@example.invalid",
                           "-c", "commit.gpgsign=false", *arguments],
                          cwd=cwd, check=True, capture_output=True, text=True).stdout.strip()


def executable(path, source):
    path.write_text(f"#!{sys.executable}\n{source}")
    path.chmod(0o755)
    return path


def b64(data):
    return base64.b64encode(data).decode()


def make_bundle(statement, der=REVIEWED_DER, signature=b"0d\x02\x30" + b"\x11" * 98):
    """A bundle with the exact shape the pinned Cosign writes, without real signatures."""
    services = release.signing_services()
    root_hash = hashlib.sha256(b"fixture tree").digest()
    log_signature = b64(services["log_key_id"][:4] + b"\x22" * 64)
    witness_signature = b64(b"\x01\x02\x03\x04" + b"\x33" * 72)
    checkpoint = (f"{services['log_origin']}\n20\n{b64(root_hash)}\n\n"
                  f"\u2014 {services['log_origin']} {log_signature}\n"
                  f"\u2014 witness.example {witness_signature}\n")
    payload_pae = release.pae(release.PAYLOAD_TYPE, statement)
    body = {"apiVersion": "0.0.2", "kind": "hashedrekord", "spec": {"hashedRekordV002": {
        "data": {"algorithm": "SHA2_384", "digest": b64(hashlib.sha384(payload_pae).digest())},
        "signature": {"content": b64(signature), "verifier": {
            "keyDetails": "PKIX_ECDSA_P384_SHA_384", "publicKey": {"rawBytes": b64(der)}}}}}}
    token = b"\x30\x82\x02\xca" + b"\x05" * 40 + b"\x04\x20" + hashlib.sha256(signature).digest() + b"\x06" * 40
    return {
        "mediaType": release.BUNDLE_MEDIA_TYPE,
        "verificationMaterial": {
            "publicKey": {"hint": b64(hashlib.sha256(der).digest())},
            "tlogEntries": [{
                "logIndex": "7", "logId": {"keyId": b64(services["log_key_id"])},
                "kindVersion": {"kind": "hashedrekord", "version": "0.0.2"},
                "inclusionProof": {"logIndex": "7", "rootHash": b64(root_hash), "treeSize": "20",
                                   "hashes": [b64(bytes([n]) * 32) for n in range(5)],
                                   "checkpoint": {"envelope": checkpoint}},
                "canonicalizedBody": b64(json.dumps(body).encode()),
            }],
            "timestampVerificationData": {"rfc3161Timestamps": [{"signedTimestamp": b64(token)}]},
        },
        "dsseEnvelope": {"payload": b64(statement), "payloadType": release.PAYLOAD_TYPE,
                         "signatures": [{"sig": b64(signature), "keyid": ""}]},
    }


def encode(bundle):
    return json.dumps(bundle).encode()


def entry(bundle):
    return bundle["verificationMaterial"]["tlogEntries"][0]


def logged_body(bundle):
    return json.loads(base64.b64decode(entry(bundle)["canonicalizedBody"]))


def set_logged_body(bundle, body):
    entry(bundle)["canonicalizedBody"] = b64(json.dumps(body).encode())


# Single mutations of a passing bundle, each of which the profile must reject.
def _drop_proof(bundle):
    del entry(bundle)["inclusionProof"]


def _drop_checkpoint(bundle):
    del entry(bundle)["inclusionProof"]["checkpoint"]


def _drop_timestamp(bundle):
    bundle["verificationMaterial"]["timestampVerificationData"]["rfc3161Timestamps"] = []


def _drop_timestamp_data(bundle):
    del bundle["verificationMaterial"]["timestampVerificationData"]


def _two_timestamps(bundle):
    stamps = bundle["verificationMaterial"]["timestampVerificationData"]["rfc3161Timestamps"]
    stamps.append(dict(stamps[0]))


def _other_signature_timestamp(bundle):
    stamps = bundle["verificationMaterial"]["timestampVerificationData"]["rfc3161Timestamps"]
    stamps[0]["signedTimestamp"] = b64(b"\x30\x82" + b"\x04\x20" + hashlib.sha256(b"other").digest())


def _rekor_v1_entry(bundle):
    entry(bundle)["kindVersion"] = {"kind": "hashedrekord", "version": "0.0.1"}


def _second_v1_entry(bundle):
    extra = dict(entry(bundle), kindVersion={"kind": "dsse", "version": "0.0.1"},
                 logId={"keyId": "wNI9atQGlz+VWfO6LRygH4QUfY/8W4RFwiT5i5WRgB0="}, integratedTime="1700000000",
                 inclusionPromise={"signedEntryTimestamp": b64(b"set")})
    bundle["verificationMaterial"]["tlogEntries"].append(extra)


def _duplicate_entry(bundle):
    bundle["verificationMaterial"]["tlogEntries"].append(dict(entry(bundle)))


def _v1_log(bundle):
    entry(bundle)["logId"] = {"keyId": "wNI9atQGlz+VWfO6LRygH4QUfY/8W4RFwiT5i5WRgB0="}


def _integrated_time(bundle):
    entry(bundle)["integratedTime"] = "1700000000"


def _inclusion_promise(bundle):
    entry(bundle)["inclusionPromise"] = {"signedEntryTimestamp": b64(b"set")}


def _changed_payload(bundle):
    payload = bytearray(base64.b64decode(bundle["dsseEnvelope"]["payload"]))
    position = payload.index(b'"PCR0": "') + 9
    payload[position] = ord("f") if payload[position] != ord("f") else ord("e")
    bundle["dsseEnvelope"]["payload"] = b64(bytes(payload))


def _sha256_digest(bundle):
    body = logged_body(bundle)
    data = body["spec"]["hashedRekordV002"]["data"]
    data["algorithm"] = "SHA2_256"
    data["digest"] = b64(hashlib.sha256(base64.b64decode(bundle["dsseEnvelope"]["payload"])).digest())
    set_logged_body(bundle, body)


def _payload_digest(bundle):
    body = logged_body(bundle)
    body["spec"]["hashedRekordV002"]["data"]["digest"] = b64(
        hashlib.sha384(base64.b64decode(bundle["dsseEnvelope"]["payload"])).digest())
    set_logged_body(bundle, body)


def _other_logged_key(bundle):
    body = logged_body(bundle)
    body["spec"]["hashedRekordV002"]["signature"]["verifier"]["publicKey"]["rawBytes"] = b64(OTHER_DER)
    set_logged_body(bundle, body)


def _p256_key_details(bundle):
    body = logged_body(bundle)
    body["spec"]["hashedRekordV002"]["signature"]["verifier"]["keyDetails"] = "PKIX_ECDSA_P256_SHA_256"
    set_logged_body(bundle, body)


def _other_logged_signature(bundle):
    body = logged_body(bundle)
    body["spec"]["hashedRekordV002"]["signature"]["content"] = b64(b"other signature")
    set_logged_body(bundle, body)


def _dsse_kind(bundle):
    body = logged_body(bundle)
    body["kind"] = "dsse"
    set_logged_body(bundle, body)


def _two_signatures(bundle):
    signatures = bundle["dsseEnvelope"]["signatures"]
    signatures.append(dict(signatures[0]))


def _certificate(bundle):
    bundle["verificationMaterial"]["x509CertificateChain"] = {"certificates": [{"rawBytes": b64(b"cert")}]}


def _certificate_instead_of_key(bundle):
    material = bundle["verificationMaterial"]
    del material["publicKey"]
    material["certificate"] = {"rawBytes": b64(b"cert")}


def _bundle_v02(bundle):
    bundle["mediaType"] = "application/vnd.dev.sigstore.bundle+json;version=0.2"


def _message_signature(bundle):
    bundle["messageSignature"] = {"signature": bundle["dsseEnvelope"]["signatures"][0]["sig"]}


def _other_checkpoint_origin(bundle):
    proof = entry(bundle)["inclusionProof"]
    proof["checkpoint"]["envelope"] = proof["checkpoint"]["envelope"].replace(
        "log2025-1.rekor.sigstore.dev", "log2099-1.example.invalid")


def _witness_only_checkpoint(bundle):
    proof = entry(bundle)["inclusionProof"]
    head, _, _ = proof["checkpoint"]["envelope"].partition("\n\n")
    proof["checkpoint"]["envelope"] = head + "\n\n\u2014 witness.example " + b64(b"\x01\x02\x03\x04" + b"\x33" * 64) + "\n"


def _checkpoint_for_other_tree(bundle):
    proof = entry(bundle)["inclusionProof"]
    proof["rootHash"] = b64(hashlib.sha256(b"other tree").digest())


def _proof_for_other_entry(bundle):
    entry(bundle)["inclusionProof"]["logIndex"] = str(int(entry(bundle)["logIndex"]) + 1)


MUTATIONS = {
    "no inclusion proof": _drop_proof, "no checkpoint": _drop_checkpoint, "no timestamp": _drop_timestamp,
    "no timestamp data": _drop_timestamp_data, "two timestamps": _two_timestamps,
    "timestamp over another signature": _other_signature_timestamp, "Rekor v1 entry": _rekor_v1_entry,
    "extra v1 entry": _second_v1_entry, "two entries": _duplicate_entry, "v1 log": _v1_log,
    "integrated time": _integrated_time, "inclusion promise": _inclusion_promise,
    "tampered statement": _changed_payload, "SHA-256 digest": _sha256_digest,
    "digest of payload, not PAE": _payload_digest, "other signer key": _other_logged_key,
    "P-256 key details": _p256_key_details, "logged signature differs": _other_logged_signature,
    "dsse entry kind": _dsse_kind, "two DSSE signatures": _two_signatures, "certificate added": _certificate,
    "certificate instead of key": _certificate_instead_of_key, "bundle v0.2": _bundle_v02,
    "message signature": _message_signature, "checkpoint from another log": _other_checkpoint_origin,
    "checkpoint without log signature": _witness_only_checkpoint, "proof for another tree": _checkpoint_for_other_tree,
    "proof for another entry": _proof_for_other_entry,
}


class BundleProfileTests(unittest.TestCase):
    def setUp(self):
        self.statement = release.statement_bytes("development", "1" * 40, MEASUREMENTS, 42, 1, "d" * 64)

    def test_real_cosign_rekor_v2_bundle_meets_the_profile(self):
        # Pinned Cosign output for a synthetic statement and a throwaway P-384 key,
        # logged in Sigstore's production Rekor v2 log with a public timestamp.
        data = FIXTURE.read_bytes()
        bundle = json.loads(data)
        statement = base64.b64decode(bundle["dsseEnvelope"]["payload"])
        key = base64.b64decode(logged_body(bundle)["spec"]["hashedRekordV002"]["signature"]["verifier"]
                               ["publicKey"]["rawBytes"])
        summary = release.check_bundle_profile(data, statement, key)
        self.assertEqual(summary["log"], "https://log2025-1.rekor.sigstore.dev")
        self.assertEqual(summary["timestamps"], 1)
        self.assertEqual(json.loads(statement)["predicateType"], release.PREDICATE_TYPE)
        with self.assertRaisesRegex(release.EvidenceError, "reviewed signing key"):
            release.check_bundle_profile(data, statement, REVIEWED_DER)
        for name, mutate in MUTATIONS.items():
            if name in ("other signer key",):
                continue
            with self.subTest(mutation=name):
                changed = json.loads(data)
                mutate(changed)
                with self.assertRaises(release.EvidenceError):
                    release.check_bundle_profile(encode(changed), statement, key)

    def test_the_synthetic_shape_passes_and_every_single_mutation_fails(self):
        release.check_bundle_profile(encode(make_bundle(self.statement)), self.statement, REVIEWED_DER)
        for name, mutate in MUTATIONS.items():
            with self.subTest(mutation=name):
                bundle = make_bundle(self.statement)
                mutate(bundle)
                with self.assertRaises(release.EvidenceError):
                    release.check_bundle_profile(encode(bundle), self.statement, REVIEWED_DER)

    def test_signed_statement_must_be_byte_identical_and_the_json_unambiguous(self):
        data = encode(make_bundle(self.statement))
        reformatted = json.dumps(json.loads(self.statement)).encode()
        with self.assertRaisesRegex(release.EvidenceError, "differs from the prepared statement"):
            release.check_bundle_profile(data, reformatted, REVIEWED_DER)
        duplicate = data.replace(b'"mediaType": ', b'"mediaType": "x", "mediaType": ', 1)
        with self.assertRaisesRegex(release.EvidenceError, "Duplicate JSON key"):
            release.check_bundle_profile(duplicate, self.statement, REVIEWED_DER)
        with self.assertRaisesRegex(release.EvidenceError, "size limit"):
            release.check_bundle_profile(data + b" " * release.MAX_BUNDLE, self.statement, REVIEWED_DER)

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
        self.assertEqual({mode: (chosen.environment, chosen.bucket, chosen.host)
                          for mode, chosen in release.TARGETS.items()},
                         {"dev": ("development", "maple-attestations-dev", "attestations-dev.maple.ai"),
                          "prod": ("production", "maple-attestations-prod", "attestations.maple.ai")})
        for mode in ("development", "Dev", "", "staging"):
            with self.subTest(mode=mode), self.assertRaises(release.EvidenceError):
                release.target(mode)

    def test_statement_carries_the_settled_predicate(self):
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


class Workspace:
    """A master checkout, candidate files and fakes for gh, Cosign and aws."""

    def __init__(self, test):
        temporary = tempfile.TemporaryDirectory()
        test.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()
        self.repo = self.root / "repo"
        self.repo.mkdir()
        git("init", "-q", "--initial-branch=master", cwd=self.repo)
        (self.repo / "source").write_text("build source\n")
        git("add", "-A", cwd=self.repo)
        git("commit", "-q", "-m", "build", cwd=self.repo)
        self.source_sha = git("rev-parse", "HEAD", cwd=self.repo)
        (self.repo / "later").write_text("later master change\n")
        git("add", "-A", cwd=self.repo)
        git("commit", "-q", "-m", "later", cwd=self.repo)
        key = self.repo / "scripts/ci/opensecret-release/dev-signing-key.pem"
        key.parent.mkdir(parents=True)
        key.write_text(pem(REVIEWED_DER))
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.state = self.root / "state"
        self.state.mkdir()
        self.storage = self.root / "storage"
        self.storage.mkdir()
        self.candidate = self.root / "candidate"
        self.release = self.root / "release"
        self.run_id = 4242
        self.files = {"image.eif": b"fixture EIF, not a real image", "pcr.json": json.dumps(MEASUREMENTS).encode()}
        self.handoff = {"environment": "dev", "source_sha": self.source_sha, "measurements": MEASUREMENTS,
                        "eif_sha256": hashlib.sha256(self.files["image.eif"]).hexdigest(), "approval_needed": True}
        self.run = {"id": self.run_id, "run_attempt": 1, "path": release.BUILD_WORKFLOW, "event": "workflow_dispatch",
                    "head_branch": "master", "head_sha": self.source_sha, "status": "completed",
                    "conclusion": "success", "repository": {"full_name": release.REPOSITORY, "private": False},
                    "head_repository": {"full_name": release.REPOSITORY}}
        self.artifact = {"id": 99, "name": f"opensecret-eif-dev-{self.source_sha[:12]}", "expired": False,
                         "size_in_bytes": 4096, "workflow_run": {"id": self.run_id, "head_sha": self.source_sha}}
        self.attempt = 1
        self.attested = None  # defaults to the three candidate files
        self.extra_zip_entries = {}
        self.write_gh()
        self.write_cosign()
        self.write_aws()
        self.environ = {
            "PATH": f"{self.bin}:{os.environ['PATH']}", "HOME": str(self.root), "TMPDIR": str(self.root),
            "SIGNING_KEY_ARN": KEY_ARN, "SIGNER_ROLE_ARN": ROLE_ARN,
            "SIGNING_PUBLIC_KEY": "scripts/ci/opensecret-release/dev-signing-key.pem",
            "GITHUB_RUN_ID": "777", "GITHUB_OUTPUT": str(self.root / "github-output"),
            "GITHUB_STEP_SUMMARY": str(self.root / "summary.md"),
            "ATTESTATIONS_R2_ACCOUNT_ID": R2_ACCOUNT, "ATTESTATIONS_R2_BUCKET": "maple-attestations-dev",
            "AWS_ACCESS_KEY_ID": "fixture-access-key", "AWS_SECRET_ACCESS_KEY": "fixture-secret-key",
            "AWS_SESSION_TOKEN": "inherited-session-token", "AWS_PROFILE": "inherited-profile",
        }
        cosign = self.bin / "cosign"
        patches = [
            mock.patch.object(release, "ROOT", self.repo),
            mock.patch.dict(os.environ, self.environ, clear=True),
            mock.patch.object(release, "cosign_platform", return_value="linux-amd64"),
            mock.patch.dict(release.COSIGN_SHA256, {"linux-amd64": release.sha256_file(cosign)}),
            mock.patch.object(release, "github_oidc_token", return_value="header.claims.signature"),
            mock.patch.object(release, "READBACK_DELAY", 0),
        ]
        for patch in patches:
            patch.start()
            test.addCleanup(patch.stop)
        test.enterContext(contextlib.redirect_stdout(io.StringIO()))

    # -- fakes ---------------------------------------------------------------

    def write_gh(self):
        executable(self.bin / "gh", f"""
import hashlib, json, sys
from pathlib import Path
state = Path({str(self.state)!r})
args = sys.argv[1:]
with (state / "gh.log").open("a") as log:
    log.write(json.dumps(args) + "\\n")
config = json.loads((state / "gh.json").read_text())
if args[:2] == ["attestation", "verify"]:
    digest = hashlib.sha256(Path(args[2]).read_bytes()).hexdigest()
    found = [item for item in config["attestations"] if digest in item["digests"]]
    if not found:
        sys.exit(1)
    print(json.dumps([{{"verificationResult": {{"statement": item["statement"]}}}} for item in found]))
elif args[0] == "api" and "--paginate" in args:
    print(json.dumps(config["artifacts"]))
elif args[0] == "api" and args[1].endswith("/zip"):
    sys.stdout.buffer.write((state / "artifact.zip").read_bytes())
elif args[0] == "api":
    print(json.dumps(config["run"]))
else:
    sys.exit("unexpected gh call")
""")

    def write_cosign(self):
        executable(self.bin / "cosign", f"""
import json, os, shutil, sys
from pathlib import Path
state = Path({str(self.state)!r})
args = sys.argv[1:]
env = {{key: value for key, value in os.environ.items() if key.startswith(("AWS_", "TUF_"))}}
token = env.get("AWS_WEB_IDENTITY_TOKEN_FILE")
with (state / "cosign.log").open("a") as log:
    log.write(json.dumps({{"args": args, "env": env,
                          "token": Path(token).read_text() if token and Path(token).exists() else None}}) + "\\n")
config = json.loads((state / "cosign.json").read_text()) if (state / "cosign.json").exists() else {{}}
if args[0] == "public-key":
    sys.stdout.write((state / "served.pem").read_text())
elif args[0] == "attest-blob":
    if config.get("attest_fails"):
        sys.exit(1)
    shutil.copyfile(state / "signed-bundle.json", args[args.index("--bundle") + 1])
elif args[0] == "verify-blob-attestation":
    sys.exit(1 if config.get("verify_fails") else 0)
else:
    sys.exit("unexpected cosign call")
""")
        (self.state / "served.pem").write_text(pem(REVIEWED_DER))

    def write_aws(self):
        executable(self.bin / "aws", f"""
import json, os, sys
from pathlib import Path
state = Path({str(self.state)!r})
storage = Path({str(self.storage)!r})
args = sys.argv[1:]
with (state / "aws.log").open("a") as log:
    log.write(json.dumps({{"args": args, "env": sorted(key for key in os.environ if key.startswith("AWS_"))}}) + "\\n")
if (state / "aws-fails").exists():
    sys.exit(1)
option = lambda name: args[args.index(name) + 1]
assert args[:2] == ["s3api", "put-object"], args
target = storage / option("--bucket") / option("--key")
target.parent.mkdir(parents=True, exist_ok=True)
target.write_bytes(Path(option("--body")).read_bytes())
Path(str(target) + ".meta").write_text(json.dumps({{"type": option("--content-type"), "cache": option("--cache-control")}}))
""")

    def log(self, tool):
        path = self.state / f"{tool}.log"
        return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []

    # -- build evidence --------------------------------------------------------

    def candidate_files(self, handoff=None, sums=None, files=None):
        files = dict(files or self.files)
        files["handoff.json"] = json.dumps(handoff or self.handoff).encode()
        names = sums or ("image.eif", "pcr.json", "handoff.json")
        files["SHA256SUMS"] = "".join(f"{hashlib.sha256(files[name]).hexdigest()}  {name}\n"
                                      for name in names).encode()
        return files

    def stage(self, handoff=None, sums=None, shipped=None):
        """Write the gh fixture: what was attested at build time, and what the artifact now holds."""
        attested = self.candidate_files(handoff, sums)
        shipped = shipped or attested
        with zipfile.ZipFile(self.state / "artifact.zip", "w") as archive:
            for name, data in {**shipped, **self.extra_zip_entries}.items():
                archive.writestr(name, data)
        names = self.attested or [line.split("  ")[1] for line in attested["SHA256SUMS"].decode().splitlines()]
        statement = {"_type": release.STATEMENT_TYPE, "predicateType": release.SLSA_TYPE,
                     "subject": [{"name": name, "digest": {"sha256": hashlib.sha256(attested[name]).hexdigest()}}
                                 for name in names],
                     "predicate": {"runDetails": {"metadata": {"invocationId":
                         f"{release.SOURCE_REPOSITORY}/actions/runs/{self.run_id}/attempts/{self.attempt}"}}}}
        (self.state / "gh.json").write_text(json.dumps({
            "run": self.run, "artifacts": [{"total_count": 1, "artifacts": [self.artifact]}],
            "attestations": [{"digests": [item["digest"]["sha256"] for item in statement["subject"]],
                              "statement": statement}],
        }))

    def prepare(self):
        self.stage_if_needed()
        return release.prepare("dev", str(self.run_id), self.candidate, self.release)

    def stage_if_needed(self):
        if not (self.state / "gh.json").exists():
            self.stage()

    def statement(self):
        return (self.release / "statement.json").read_bytes()

    def signed_bundle(self, mutate=None):
        bundle = make_bundle(self.statement())
        if mutate:
            mutate(bundle)
        (self.state / "signed-bundle.json").write_bytes(encode(bundle))

    def cosign_config(self, **values):
        (self.state / "cosign.json").write_text(json.dumps(values))


class PrepareTests(unittest.TestCase):
    def setUp(self):
        self.work = Workspace(self)

    def test_verified_master_build_becomes_the_exact_statement(self):
        self.work.run["run_attempt"] = 2  # a later attempt re-ran only another job
        self.work.prepare()
        statement = json.loads(self.work.statement())
        self.assertEqual(statement["subject"], [{"name": "image.eif", "digest": {"sha256": self.work.handoff[
            "eif_sha256"]}}])
        self.assertEqual(statement["predicate"], {
            "environment": "development",
            "source": {"repository": "https://github.com/MaplePrivacyLabs/Maple", "commit": self.work.source_sha},
            "measurements": {"PCR0": "a" * 96, "PCR1": "b" * 96, "PCR2": "c" * 96},
            "build": {"runId": str(self.work.run_id), "runAttempt": 1},
        })
        self.assertEqual(sorted(p.name for p in self.work.candidate.iterdir()),
                         ["SHA256SUMS", "handoff.json", "image.eif", "pcr.json"])
        self.assertFalse((self.work.release / "artifact.zip").exists())
        outputs = dict(line.split("=", 1) for line in (self.work.root / "github-output").read_text().splitlines())
        self.assertEqual(outputs, {"source_sha": self.work.source_sha, "eif_sha256": self.work.handoff["eif_sha256"],
                                   "pcr0": "a" * 96})
        verify = [args for args in self.work.log("gh") if args[:2] == ["attestation", "verify"]]
        self.assertEqual(sorted(Path(args[2]).name for args in verify), ["handoff.json", "image.eif", "pcr.json"])
        for args in verify:
            options = dict(zip(args[3::2], args[4::2]))
            self.assertEqual(options, {
                "--repo": "MaplePrivacyLabs/Maple",
                "--cert-identity": "https://github.com/MaplePrivacyLabs/Maple/.github/workflows/"
                                   "opensecret-eif-release.yml@refs/heads/master",
                "--cert-oidc-issuer": "https://token.actions.githubusercontent.com",
                "--source-ref": "refs/heads/master", "--source-digest": self.work.source_sha,
                "--signer-digest": self.work.source_sha, "--predicate-type": "https://slsa.dev/provenance/v1",
                "--format": "json"})

    def assert_refused(self, message=None):
        with self.assertRaises(release.EvidenceError) as caught:
            self.work.prepare()
        if message:
            self.assertRegex(str(caught.exception), message)
        self.assertFalse((self.work.release / "statement.json").exists())

    def test_runs_other_than_a_successful_master_dispatch_of_the_release_workflow_are_refused(self):
        wrong = {
            "path": ".github/workflows/opensecret-eif.yml", "event": "push", "head_branch": "feature",
            "conclusion": "failure", "status": "in_progress", "head_sha": "abc",
            "head_repository": {"full_name": "fork/Maple"},
            "repository": {"full_name": "fork/Maple", "private": False},
        }
        for field, value in wrong.items():
            with self.subTest(field=field):
                self.work = Workspace(self)
                self.work.run[field] = value
                self.assert_refused()
                self.assertFalse(self.work.candidate.exists())

    def test_build_commit_outside_master_history_is_refused(self):
        git("checkout", "-q", "-b", "side", self.work.source_sha, cwd=self.work.repo)
        (self.work.repo / "side").write_text("unreviewed\n")
        git("add", "-A", cwd=self.work.repo)
        git("commit", "-q", "-m", "side", cwd=self.work.repo)
        side = git("rev-parse", "HEAD", cwd=self.work.repo)
        git("checkout", "-q", "master", cwd=self.work.repo)
        self.work.run["head_sha"] = side
        self.work.handoff["source_sha"] = side
        self.work.artifact["workflow_run"]["head_sha"] = side
        self.work.artifact["name"] = f"opensecret-eif-dev-{side[:12]}"
        self.assert_refused("not in this master checkout")

    def test_artifact_must_be_the_runs_own_unexpired_dev_artifact(self):
        cases = {
            "another run": lambda artifact: artifact["workflow_run"].update(id=1),
            "another commit": lambda artifact: artifact["workflow_run"].update(head_sha="2" * 40),
            "expired": lambda artifact: artifact.update(expired=True),
            "prod artifact name": lambda artifact: artifact.update(name=artifact["name"].replace("-dev-", "-prod-")),
        }
        for name, change in cases.items():
            with self.subTest(case=name):
                self.work = Workspace(self)
                change(self.work.artifact)
                self.assert_refused()

    def test_changed_or_extra_artifact_files_are_refused(self):
        changed = self.work.candidate_files()
        changed["image.eif"] = b"substituted EIF"
        self.work.stage(shipped=changed)
        self.assert_refused("differ from the build's SHA256SUMS")
        # A substituted EIF with matching checksums still lacks this run's provenance.
        self.work = Workspace(self)
        self.work.stage(shipped=self.work.candidate_files(files={**self.work.files, "image.eif": b"substituted"}))
        self.assert_refused("provenance for image.eif")
        self.work = Workspace(self)
        self.work.extra_zip_entries = {"extra.sh": b"echo"}
        self.work.stage()
        self.assert_refused("exactly image.eif")

    def test_builds_made_before_the_handoff_was_attested_are_refused(self):
        self.work.stage(sums=("image.eif", "pcr.json"))
        self.assert_refused("predates attested handoffs")

    def test_provenance_must_cover_all_three_files_for_this_run(self):
        self.work.attested = ["image.eif", "pcr.json"]
        self.work.stage()
        self.assert_refused("does not cover")
        self.work = Workspace(self)
        self.work.run_id = 5000  # provenance from another run of the same workflow
        self.work.stage()
        self.work.run_id = 4242
        self.assert_refused("does not cover")
        self.work = Workspace(self)
        self.work.attempt = 2  # an attempt the selected run never reached
        self.work.stage()
        self.assert_refused("does not cover")

    def test_handoff_must_say_dev_and_agree_with_the_artifact(self):
        cases = {
            "prod build": {"environment": "prod"},
            "other commit": {"source_sha": "3" * 40},
            "other EIF": {"eif_sha256": "0" * 64},
            "other measurements": {"measurements": {**MEASUREMENTS, "PCR1": "e" * 96}},
            "extra field": {"environment": "dev", "signed_environment": "production"},
        }
        for name, change in cases.items():
            with self.subTest(case=name):
                self.work = Workspace(self)
                self.work.stage(handoff={**self.work.handoff, **change})
                self.assert_refused()

    def test_malformed_measurements_are_refused(self):
        self.work.files["pcr.json"] = json.dumps({**MEASUREMENTS, "PCR0": "0" * 96}).encode()
        self.work.handoff["measurements"] = json.loads(self.work.files["pcr.json"])
        self.assert_refused("Invalid PCR0")


class SignTests(unittest.TestCase):
    def setUp(self):
        self.work = Workspace(self)
        self.work.prepare()
        self.cosign = self.work.bin / "cosign"

    def sign(self):
        return release.sign("dev", self.work.candidate, self.work.release, self.cosign)

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
        for call in calls[:2]:
            # Only the role assumed through GitHub's OIDC token reaches KMS.
            self.assertEqual(call["env"]["AWS_ROLE_ARN"], ROLE_ARN)
            self.assertEqual(call["env"]["AWS_REGION"], "us-east-1")
            self.assertEqual(call["token"], "header.claims.signature")
            self.assertFalse({"AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY", "AWS_SESSION_TOKEN", "AWS_PROFILE"}
                             & set(call["env"]))
            self.assertFalse(Path(call["env"]["AWS_WEB_IDENTITY_TOKEN_FILE"]).exists())  # removed afterwards
        self.assertEqual(calls[2]["args"], [
            "verify-blob-attestation", "--bundle", str(self.work.release / "bundle.json"),
            "--key", str(self.work.repo / "scripts/ci/opensecret-release/dev-signing-key.pem"),
            "--type", release.PREDICATE_TYPE, "--use-signed-timestamps", "--trusted-root", str(release.TRUSTED_ROOT),
            str(self.work.candidate / "image.eif")])
        receipt = json.loads((self.work.release / "bundle.verification.json").read_text())
        self.assertEqual(receipt["cosign"], "v3.1.4")
        self.assertEqual(receipt["keyArn"], KEY_ARN)

    def assert_never_signed(self, message=None):
        with self.assertRaises(release.EvidenceError) as caught:
            self.sign()
        if message:
            self.assertRegex(str(caught.exception), message)
        self.assertNotIn("attest-blob", [call["args"][0] for call in self.work.log("cosign")])
        self.assertFalse((self.work.release / "bundle.json").exists())

    def test_placeholder_or_unfixed_identity_never_reaches_cosign(self):
        workflow = workflows()["opensecret-release-statement-dev.yml"]["jobs"]["sign"]["env"]
        configured = {name: workflow[name] for name in ("SIGNING_KEY_ARN", "SIGNER_ROLE_ARN", "SIGNING_PUBLIC_KEY")}
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
        # The workflow's own values either form a complete identity or are refused here too.
        complete = not any("REPLACE" in value for value in configured.values()) and (
            ROOT / configured["SIGNING_PUBLIC_KEY"]).is_file()
        if not complete:
            cases["the workflow's configured identity"] = configured
        (self.work.root / "outside.pem").write_text(pem(REVIEWED_DER))
        self.work.signed_bundle()
        for name, values in cases.items():
            with self.subTest(case=name), mock.patch.dict(os.environ, values):
                self.assert_never_signed()
                self.assertEqual(self.work.log("cosign"), [])
                with contextlib.redirect_stderr(io.StringIO()):
                    self.assertEqual(release.main(["preflight", "dev"]), 1)

    def test_kms_key_other_than_the_reviewed_key_is_refused_before_signing(self):
        (self.work.state / "served.pem").write_text(pem(OTHER_DER))
        self.work.signed_bundle()
        self.assert_never_signed("differs from the reviewed public key")

    def test_tampered_statement_or_changed_eif_is_refused_before_signing(self):
        original = self.work.statement()
        tampered = original.replace(b'"development"', b'"production"')
        (self.work.release / "statement.json").write_bytes(tampered)
        self.work.signed_bundle()
        self.assert_never_signed("statement was modified")
        (self.work.release / "statement.json").write_bytes(original)
        (self.work.candidate / "image.eif").write_bytes(b"changed after verification")
        self.assert_never_signed("SHA256SUMS")

    def test_an_unpinned_cosign_binary_is_refused(self):
        self.work.signed_bundle()
        with mock.patch.dict(release.COSIGN_SHA256, {"linux-amd64": "0" * 64}):
            self.assert_never_signed("not the pinned")

    def test_missing_proof_timestamp_or_extra_entries_fail_and_nothing_is_published(self):
        for name in ("no inclusion proof", "no timestamp", "Rekor v1 entry", "extra v1 entry", "inclusion promise",
                     "tampered statement", "other signer key"):
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

    def test_cosign_failures_fail_the_job(self):
        self.work.signed_bundle()
        self.work.cosign_config(attest_fails=True)
        with self.assertRaises(subprocess.CalledProcessError):
            self.sign()
        self.assertFalse((self.work.release / "bundle.json").exists())
        self.work.cosign_config(verify_fails=True)
        with self.assertRaises(subprocess.CalledProcessError):
            self.sign()
        self.assertFalse((self.work.release / "bundle.verification.json").exists())


class Host(http.server.BaseHTTPRequestHandler):
    """The public bucket host: serves stored objects with their stored metadata."""

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
        self.work = Workspace(self)
        self.work.prepare()
        self.work.signed_bundle()
        self.cosign = self.work.bin / "cosign"
        release.sign("dev", self.work.candidate, self.work.release, self.cosign)
        handler = type("FixtureHost", (Host,), {"storage": self.work.storage, "overrides": {}, "origins": []})
        self.handler = handler
        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        self.addCleanup(thread.join)
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        self.base_url = f"http://127.0.0.1:{server.server_address[1]}"
        self.key = f"{'a' * 96}.json"
        self.bucket = self.work.storage / "maple-attestations-dev"

    def publish(self):
        return release.publish("dev", self.work.candidate, self.work.release, self.cosign, base_url=self.base_url,
                               sleep=lambda _: None)

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
        cases = {"stale bytes": {"body": b"{}"}, "wrong cache policy": {"cache": "public, max-age=31536000, immutable"},
                 "no CORS": {"cors": False}}
        for name, override in cases.items():
            with self.subTest(case=name):
                self.handler.overrides = override
                self.assert_not_published("was not confirmed")
        self.assertEqual(len(self.handler.origins), len(cases) * release.READBACK_ATTEMPTS)

    def test_publisher_is_bound_to_the_dev_bucket_and_needs_its_credential(self):
        cases = {"prod bucket": {"ATTESTATIONS_R2_BUCKET": "maple-attestations-prod"},
                 "bad account": {"ATTESTATIONS_R2_ACCOUNT_ID": "not-an-account"},
                 "no credential": {"AWS_SECRET_ACCESS_KEY": ""}}
        for name, values in cases.items():
            with self.subTest(case=name), mock.patch.dict(os.environ, values):
                self.assert_not_published("")
        self.assertEqual(self.work.log("aws"), [])

    def test_a_bundle_changed_after_signing_is_never_uploaded(self):
        bundle = json.loads((self.work.release / "bundle.json").read_text())
        _changed_payload(bundle)
        (self.work.release / "bundle.json").write_bytes(encode(bundle))
        self.assert_not_published("differs from the prepared statement")
        self.assertEqual(self.work.log("aws"), [])


if __name__ == "__main__":
    unittest.main()
