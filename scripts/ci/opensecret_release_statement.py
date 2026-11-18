"""Verify one OpenSecret EIF build, sign its release statement and publish the bundle.

The reviewer-gated job in opensecret-release-statement-dev.yml runs these
subcommands in order. Each one re-checks what it relies on and fails closed:

  preflight MODE                     the fixed signing identity is complete
  prepare MODE RUN_ID CANDIDATE RELEASE
                                     the build run, artifact and build provenance
                                     are the selected master release build; writes
                                     the exact in-toto statement to sign
  install-cosign DEST                the pinned Cosign release, digest-checked
  sign MODE CANDIDATE RELEASE COSIGN signs with the fixed KMS key through the
                                     pinned signing configuration and verifies the
                                     bundle profile before anything is published
  publish MODE CANDIDATE RELEASE COSIGN
                                     uploads the bundle to the environment's bucket,
                                     reads it back from the public host and verifies
                                     the public bytes

The environment is fixed per workflow file; nothing a caller supplies, and no
field of a bundle, chooses it. Cryptographic verification of the bundle belongs
to Cosign. This script enforces Maple's narrower evidence profile on top of it.
See services/opensecret/docs/release-statements.md.
"""

from __future__ import annotations

import argparse
import base64
import binascii
from dataclasses import dataclass
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import stat
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
import zipfile

ROOT = Path(__file__).resolve().parents[2]
CONFIG = Path(__file__).resolve().parent / "opensecret-release"
SIGNING_CONFIG = CONFIG / "signing-config.v0.2.json"
TRUSTED_ROOT = CONFIG / "trusted_root.json"

REPOSITORY = "MaplePrivacyLabs/Maple"
SOURCE_REPOSITORY = f"https://github.com/{REPOSITORY}"
BUILD_WORKFLOW = ".github/workflows/opensecret-eif-release.yml"
BUILD_IDENTITY = f"{SOURCE_REPOSITORY}/{BUILD_WORKFLOW}@refs/heads/master"
OIDC_ISSUER = "https://token.actions.githubusercontent.com"
SLSA_TYPE = "https://slsa.dev/provenance/v1"

STATEMENT_TYPE = "https://in-toto.io/Statement/v1"
PREDICATE_TYPE = "https://maple.ai/attestations/enclave-release/v1"
PAYLOAD_TYPE = "application/vnd.in-toto+json"
BUNDLE_MEDIA_TYPE = "application/vnd.dev.sigstore.bundle.v0.3+json"
SUBJECT_NAME = "image.eif"

COSIGN_VERSION = "v3.1.4"
COSIGN_URL = "https://github.com/sigstore/cosign/releases/download/{version}/cosign-{platform}"
COSIGN_SHA256 = {
    "linux-amd64": "424e2c97d0b45fd7a6b608a87f090f35633f4c23ece5f092d48bba11aaf4bbfa",
    "linux-arm64": "ade83b7aac62e131cacfe96d958948b2150cc9efec46a570a2649b8e27dedad2",
}

KMS_REGION = "us-east-1"
KEY_ARN = re.compile(r"arn:aws:kms:us-east-1:(?P<account>[0-9]{12}):key/"
                     r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}")
ROLE_ARN = re.compile(r"arn:aws:iam::(?P<account>[0-9]{12}):role/(?P<name>[A-Za-z0-9+=,.@_-]+)")
# DER prefix of a SubjectPublicKeyInfo for an uncompressed P-384 point, and the
# point itself: 0x04 followed by 48-byte X and Y coordinates.
P384_SPKI_PREFIX = bytes.fromhex("3076301006072a8648ce3d020106052b81040022036200")
P384_POINT_BYTES = 1 + 2 * 48
# A signed-note signature line starts with the first four bytes of the signer's key ID.
NOTE_KEY_HASH_BYTES = 4

CACHE_CONTROL = "public, max-age=300"
# The browser origin used for the readback; the bucket's CORS rule allows any.
READBACK_ORIGIN = "https://trymaple.ai"
READBACK_ATTEMPTS = 12
READBACK_DELAY = 30

FILES = ("image.eif", "pcr.json", "handoff.json")
MEASUREMENTS = ("PCR0", "PCR1", "PCR2")
MAX_EIF_BYTES = 2 * 1024 * 1024 * 1024
# pcr.json, handoff.json, SHA256SUMS, the recorded candidate and API responses.
MAX_SMALL_FILE_BYTES = 64 * 1024
MAX_PUBLIC_KEY_BYTES = 4096
MAX_BUNDLE_BYTES = 1024 * 1024
MAX_COSIGN_BYTES = 256 * 1024 * 1024


@dataclass(frozen=True)
class Target:
    """Everything one approval workflow fixes. Build mode maps explicitly to the signed environment."""

    mode: str
    environment: str
    bucket: str
    host: str
    signer_role: str


TARGETS = {
    "dev": Target("dev", "development", "maple-attestations-dev", "attestations-dev.maple.ai", "maple-pcr-signer-dev"),
    "prod": Target("prod", "production", "maple-attestations-prod", "attestations.maple.ai", "maple-pcr-signer-prod"),
}


class EvidenceError(ValueError):
    """Evidence did not meet a requirement; nothing after this point may run."""


def require(condition, message):
    if not condition:
        raise EvidenceError(message)


def json_object(value, what):
    """value as a JSON object. An absent or empty value reads as an empty object; any other value is malformed."""
    require(isinstance(value, dict) or not value, f"{what} is not a JSON object")
    return value or {}


def is_hex(value, length):
    return isinstance(value, str) and re.fullmatch(f"[0-9a-f]{{{length}}}", value) is not None


def sha256_file(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def strict_json(data):
    """Parse JSON bytes, rejecting duplicate keys so no two readers can disagree."""

    def unique(pairs):
        result = {}
        for key, value in pairs:
            require(key not in result, f"Duplicate JSON key {key!r}")
            result[key] = value
        return result

    def constant(name):
        raise EvidenceError(f"Non-standard JSON constant {name}")

    try:
        return json.loads(data, object_pairs_hook=unique, parse_constant=constant)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise EvidenceError(f"Invalid JSON: {error}") from None


def read_json(path, limit=MAX_SMALL_FILE_BYTES):
    require(path.is_file() and not path.is_symlink(), f"Expected a regular file: {path.name}")
    require(path.stat().st_size <= limit, f"{path.name} exceeds its size limit")
    return strict_json(path.read_bytes())


def write_json(path, value):
    path.write_text(json.dumps(value, indent=2) + "\n")


def b64decode(value, what):
    require(isinstance(value, str) and value, f"Missing {what}")
    try:
        return base64.b64decode(value, validate=True)
    except binascii.Error:
        raise EvidenceError(f"{what} is not base64") from None


def clean_env(**extra):
    """No inherited cloud credentials, profiles or metadata lookups reach a tool."""
    env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "HOME": os.environ.get("HOME", "/nonexistent"),
           "AWS_EC2_METADATA_DISABLED": "true", "AWS_CONFIG_FILE": os.devnull,
           "AWS_SHARED_CREDENTIALS_FILE": os.devnull}
    if os.environ.get("TMPDIR"):
        env["TMPDIR"] = os.environ["TMPDIR"]
    env.update(extra)
    return env


def github_output(**values):
    if os.environ.get("GITHUB_OUTPUT"):
        with open(os.environ["GITHUB_OUTPUT"], "a") as output:
            for key, value in values.items():
                output.write(f"{key}={value}\n")


def step_summary(text):
    if os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(os.environ["GITHUB_STEP_SUMMARY"], "a") as summary:
            summary.write(text)


# --- Fixed signing identity -------------------------------------------------


def target_for(mode):
    require(mode in TARGETS, f"Unknown build mode {mode!r}")
    return TARGETS[mode]


def public_key_der(text):
    """SPKI DER of a single P-384 PEM public key."""
    match = re.fullmatch(r"-----BEGIN PUBLIC KEY-----\n([A-Za-z0-9+/=\n]+)-----END PUBLIC KEY-----\n?", text)
    require(match, "Expected one PEM public key")
    der = b64decode("".join(match.group(1).split()), "public key")
    require(len(der) == len(P384_SPKI_PREFIX) + P384_POINT_BYTES and der.startswith(P384_SPKI_PREFIX + b"\x04"),
            "Expected an uncompressed P-384 public key")
    return der


def committed_file(relative):
    """The bytes HEAD records for a file at this repository path, or None if HEAD has no file there."""
    listing = subprocess.run(["git", "ls-tree", "-z", "HEAD", "--", relative], cwd=ROOT,
                             stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    require(listing.returncode == 0, "Cannot read the checked-out commit's tree")
    for entry in listing.stdout.split(b"\0"):
        metadata, _, name = entry.partition(b"\t")
        if name == relative.encode() and metadata.split(b" ")[1:2] == [b"blob"]:
            blob = subprocess.run(["git", "cat-file", "blob", metadata.split(b" ")[2].decode()], cwd=ROOT,
                                  stdout=subprocess.PIPE, check=True)
            return blob.stdout
    return None


def reviewed_public_key(environ):
    """The reviewed public key the bundle must verify against.

    It must be a file the checked-out commit tracks, unmodified in the working
    tree, and reached through no symlink: Cosign reads this path, so it must be
    exactly the file review approved.
    """
    configured = environ.get("SIGNING_PUBLIC_KEY", "")
    require(configured and not os.path.isabs(configured), "SIGNING_PUBLIC_KEY must name a repository file")
    path = (ROOT / configured).resolve()
    require(path.is_relative_to(ROOT), "SIGNING_PUBLIC_KEY must stay inside the repository")
    # Check each component as configured: resolving first would follow a link.
    component = ROOT
    for part in Path(configured).parts:
        component /= part
        require(not component.is_symlink(),
                f"The reviewed public key {configured} is reached through a symlink; refusing to sign")
    require(path.is_file(), f"The reviewed public key {configured} is not committed; refusing to sign")
    require(path.stat().st_size <= MAX_PUBLIC_KEY_BYTES, "Public key file is too large")
    committed = committed_file(path.relative_to(ROOT).as_posix())
    require(committed is not None, f"The reviewed public key {configured} is not committed; refusing to sign")
    require(path.read_bytes() == committed,
            f"The reviewed public key {configured} differs from its committed content; refusing to sign")
    return path, public_key_der(committed.decode(errors="replace"))


def signing_identity(target, environ):
    """Validate the workflow's fixed key and role. Placeholders and aliases never pass."""
    key_arn = environ.get("SIGNING_KEY_ARN", "")
    role_arn = environ.get("SIGNER_ROLE_ARN", "")
    for name, value in (("SIGNING_KEY_ARN", key_arn), ("SIGNER_ROLE_ARN", role_arn)):
        require("REPLACE" not in value.upper(), f"{name} is still a placeholder; refusing to sign")
    key = KEY_ARN.fullmatch(key_arn)
    require(key, f"SIGNING_KEY_ARN must be a full KMS key ARN in {KMS_REGION}, never an alias")
    role = ROLE_ARN.fullmatch(role_arn)
    require(role and role["name"] == target.signer_role,
            f"SIGNER_ROLE_ARN must be the {target.signer_role} role")
    require(role["account"] == key["account"], "Signer role and key must be in the same account")
    path, der = reviewed_public_key(environ)
    return key_arn, role_arn, path, der


def preflight(mode):
    target = target_for(mode)
    key_arn, role_arn, path, der = signing_identity(target, os.environ)
    print(f"Signing identity for {target.environment}: {key_arn} via {role_arn}; "
          f"reviewed key {path.relative_to(ROOT)} (SPKI SHA-256 {hashlib.sha256(der).hexdigest()}).")


# --- Build evidence ---------------------------------------------------------


def command_json(arguments):
    result = subprocess.run(arguments, check=True, stdout=subprocess.PIPE)
    return strict_json(result.stdout)


def api(path):
    return command_json(["gh", "api", f"repos/{REPOSITORY}/{path}"])


def validate_run(run, run_id):
    require(isinstance(run, dict) and str(run.get("id")) == run_id, "Build run ID mismatch")
    require(json_object(run.get("repository"), "Build run repository").get("full_name") == REPOSITORY,
            "Build must belong to the canonical repository")
    head_repository = json_object(run.get("head_repository"), "Build run head repository")
    require(head_repository.get("full_name") == REPOSITORY, "Fork builds cannot be approved")
    require(run["repository"].get("private") is False, "Public Sigstore evidence needs the public repository")
    require(run.get("path") == BUILD_WORKFLOW, "Build run is not the OpenSecret EIF release workflow")
    require(run.get("event") == "workflow_dispatch" and run.get("head_branch") == "master",
            "Only release builds dispatched on master can be approved")
    require(run.get("status") == "completed" and run.get("conclusion") == "success",
            "Build run did not complete successfully")
    require(is_hex(run.get("head_sha"), 40), "Build run has no full source commit")
    require(type(run.get("run_attempt")) is int and run["run_attempt"] > 0, "Build run has no valid attempt")


def select_artifact(pages, run, mode):
    name = f"opensecret-eif-{mode}-{run['head_sha'][:12]}"
    require(isinstance(pages, list), "Unexpected artifact listing")
    listed = []
    for page in pages:
        require(isinstance(page, dict), "Artifact listing page is not a JSON object")
        artifacts = page.get("artifacts", [])
        # GitHub sends a list. An empty string or object has always listed nothing,
        # so it still does; any other non-list value is malformed.
        require(isinstance(artifacts, list) or artifacts in ("", {}), "Artifact listing page has no artifact list")
        artifacts = artifacts or []
        require(all(isinstance(artifact, dict) for artifact in artifacts), "Listed artifact is not a JSON object")
        listed += artifacts
    found = [artifact for artifact in listed if artifact.get("name") == name]
    require(len(found) == 1, f"Expected exactly one {name} artifact in the build run")
    artifact = found[0]
    require(artifact.get("expired") is False, "The build artifact has expired")
    require(type(artifact.get("id")) is int and artifact["id"] > 0, "Invalid artifact ID")
    workflow_run = json_object(artifact.get("workflow_run"), "Artifact workflow run")
    require(workflow_run.get("id") == run["id"] and workflow_run.get("head_sha") == run["head_sha"],
            "Artifact does not belong to the selected build run")
    size = artifact.get("size_in_bytes", 0)
    require(isinstance(size, (int, float)) and 0 < size <= MAX_EIF_BYTES, "Artifact size is out of bounds")
    return artifact


def extract_candidate(archive_path, destination):
    """Unpack exactly the four build files. The archive chooses no paths, links or modes."""
    destination.mkdir()
    expected = set(FILES) | {"SHA256SUMS"}
    with zipfile.ZipFile(archive_path) as archive:
        entries = archive.infolist()
        require(len(entries) == len(expected) and {entry.filename for entry in entries} == expected,
                "Build artifact must contain exactly image.eif, pcr.json, handoff.json and SHA256SUMS")
        for entry in entries:
            kind = stat.S_IFMT(entry.external_attr >> 16)
            require(kind in (0, stat.S_IFREG), f"{entry.filename} is not a regular file")
            limit = MAX_EIF_BYTES if entry.filename == "image.eif" else MAX_SMALL_FILE_BYTES
            require(0 < entry.file_size <= limit, f"{entry.filename} is empty or too large")
            with archive.open(entry) as source, (destination / entry.filename).open("xb") as output:
                shutil.copyfileobj(source, output)
            require((destination / entry.filename).stat().st_size == entry.file_size,
                    f"{entry.filename} size differs from the archive")


def check_checksums(candidate):
    """SHA256SUMS names exactly the three build files and matches them."""
    digests = {name: sha256_file(candidate / name) for name in FILES}
    lines = (candidate / "SHA256SUMS").read_text().splitlines()
    names = sorted(line.split("  ", 1)[-1] for line in lines)
    require("handoff.json" in names,
            "Build predates attested handoffs: its build mode is unauthenticated. Build it again.")
    require(sorted(lines) == sorted(f"{digests[name]}  {name}" for name in FILES),
            "Candidate files differ from the build's SHA256SUMS")
    return digests


def invocation_attempt(statement, run_id):
    """The attempt of run_id named by a SLSA statement's invocation ID, or None for any other run."""
    predicate = json_object(statement.get("predicate"), "Build provenance predicate")
    run_details = json_object(predicate.get("runDetails"), "Build provenance run details")
    metadata = json_object(run_details.get("metadata"), "Build provenance metadata")
    invocation = metadata.get("invocationId")
    run_url = f"{SOURCE_REPOSITORY}/actions/runs/{run_id}/attempts/"
    match = re.fullmatch(re.escape(run_url) + r"([1-9][0-9]{0,3})", str(invocation))
    return int(match.group(1)) if match else None


def subjects(statement):
    found = {}
    for subject in statement.get("subject") or []:
        if isinstance(subject, dict) and isinstance(subject.get("digest"), dict):
            found.setdefault(subject.get("name"), set()).add(subject["digest"].get("sha256"))
    return found


def provenance_attempts(results, run, digests):
    """Attempts of the selected run whose verified build provenance covers all three files."""
    require(isinstance(results, list) and results, "No verified build provenance")
    attempts = set()
    for result in results:
        result = json_object(result, "Build provenance result")
        verification = json_object(result.get("verificationResult"), "Build provenance verification result")
        statement = json_object(verification.get("statement"), "Build provenance statement")
        if statement.get("_type") != STATEMENT_TYPE or statement.get("predicateType") != SLSA_TYPE:
            continue
        attempt = invocation_attempt(statement, run["id"])
        covered = subjects(statement)
        covers_every_file = all(covered.get(name) == {digest} for name, digest in digests.items())
        if attempt and attempt <= run["run_attempt"] and covers_every_file:
            attempts.add(attempt)
    return attempts


def verify_build_provenance(candidate, run, digests):
    """GitHub's keyless build attestation from the release workflow covers every candidate file."""
    common = None
    verified = {}
    for name in FILES:
        result = subprocess.run([
            "gh", "attestation", "verify", str(candidate / name), "--repo", REPOSITORY,
            "--cert-identity", BUILD_IDENTITY, "--cert-oidc-issuer", OIDC_ISSUER,
            "--source-ref", "refs/heads/master", "--source-digest", run["head_sha"],
            "--signer-digest", run["head_sha"], "--predicate-type", SLSA_TYPE, "--format", "json",
        ], stdout=subprocess.PIPE)
        # gh exits non-zero when no attestation verifies under this policy.
        require(result.returncode == 0, f"No verified release-workflow build provenance for {name}")
        results = strict_json(result.stdout)
        attempts = provenance_attempts(results, run, digests)
        require(attempts, f"Build provenance does not cover {name} for the selected run")
        common = attempts if common is None else common & attempts
        verified[name] = results
    require(common, "Build provenance for the three files names different run attempts")
    return max(common), verified


def validate_handoff(candidate, run, mode, eif_sha256):
    handoff = read_json(candidate / "handoff.json")
    measurements = read_json(candidate / "pcr.json")
    require(isinstance(handoff, dict) and set(handoff) == {
        "environment", "source_sha", "measurements", "eif_sha256", "approval_needed"}, "Unexpected handoff fields")
    require(handoff["environment"] == mode, f"Candidate was built for {handoff['environment']!r}, not {mode!r}")
    require(handoff["source_sha"] == run["head_sha"], "Candidate source differs from the build run's commit")
    require(handoff["eif_sha256"] == eif_sha256, "EIF digest differs from the handoff")
    require(type(handoff["approval_needed"]) is bool, "Invalid approval_needed")
    require(isinstance(measurements, dict) and set(measurements) == {"HashAlgorithm", *MEASUREMENTS},
            "Unexpected measurement fields")
    require(handoff["measurements"] == measurements, "Measurements differ from the handoff")
    require(isinstance(measurements["HashAlgorithm"], str), "Missing measurement hash algorithm")
    for name in MEASUREMENTS:
        require(is_hex(measurements[name], 96) and measurements[name] != "0" * 96, f"Invalid {name}")
    return {name: measurements[name] for name in MEASUREMENTS}


def statement_bytes(environment, source_commit, measurements, run_id, run_attempt, eif_sha256):
    """The exact in-toto statement Cosign signs; its bytes become the DSSE payload."""
    statement = {
        "_type": STATEMENT_TYPE,
        "subject": [{"name": SUBJECT_NAME, "digest": {"sha256": eif_sha256}}],
        "predicateType": PREDICATE_TYPE,
        "predicate": {
            "environment": environment,
            "source": {"repository": SOURCE_REPOSITORY, "commit": source_commit},
            "measurements": {name: measurements[name] for name in MEASUREMENTS},
            "build": {"runId": str(run_id), "runAttempt": run_attempt},
        },
    }
    return (json.dumps(statement, indent=2) + "\n").encode()


def prepare(mode, run_id, candidate, release):
    target = target_for(mode)
    require(re.fullmatch(r"[1-9][0-9]{0,19}", run_id), "Expected a numeric build run ID")
    require(not candidate.exists() and not release.exists(), "Output directories must not exist yet")
    run = api(f"actions/runs/{run_id}")
    validate_run(run, run_id)
    # The selected build may be older than this checkout, but must be in its master history.
    ancestor = subprocess.run(["git", "merge-base", "--is-ancestor", run["head_sha"], "HEAD"], cwd=ROOT)
    require(ancestor.returncode == 0, "Build commit is not in this master checkout's history")
    pages = command_json(["gh", "api", "--paginate", "--slurp",
                          f"repos/{REPOSITORY}/actions/runs/{run_id}/artifacts?per_page=100"])
    artifact = select_artifact(pages, run, mode)
    release.mkdir()
    download_candidate(artifact, release / "artifact.zip", candidate)
    digests = check_checksums(candidate)
    eif_sha256 = digests["image.eif"]
    attempt, provenance = verify_build_provenance(candidate, run, digests)
    measurements = validate_handoff(candidate, run, mode, eif_sha256)
    statement = statement_bytes(target.environment, run["head_sha"], measurements, run["id"], attempt, eif_sha256)
    (release / "statement.json").write_bytes(statement)
    write_json(release / "build-provenance.json", provenance)
    write_json(release / "candidate.json", {
        "mode": mode,
        "environment": target.environment,
        "sourceCommit": run["head_sha"],
        "buildRunId": str(run["id"]),
        "buildRunAttempt": attempt,
        "artifactId": artifact["id"],
        "artifactName": artifact["name"],
        "eifSha256": eif_sha256,
        "measurements": measurements,
    })
    github_output(source_sha=run["head_sha"], eif_sha256=eif_sha256, pcr0=measurements["PCR0"])
    rows = {
        "Build run": f"{run['id']} attempt {attempt}",
        "Source commit": f"`{run['head_sha']}`",
        "EIF SHA-256": f"`{eif_sha256}`",
        "PCR0": f"`{measurements['PCR0']}`",
        "Signed environment": f"`{target.environment}`",
    }
    table = "".join(f"| {field} | {value} |\n" for field, value in rows.items())
    step_summary(f"## Verified OpenSecret {mode} build\n\n| Field | Value |\n| --- | --- |\n{table}\n")
    print(f"Verified {mode} build run {run['id']} attempt {attempt}: EIF {eif_sha256}, PCR0 {measurements['PCR0']}.")


def download_candidate(artifact, archive, candidate):
    """Download the build artifact and unpack it as the candidate directory."""
    with archive.open("xb") as output:
        subprocess.run(["gh", "api", f"repos/{REPOSITORY}/actions/artifacts/{artifact['id']}/zip"],
                       stdout=output, check=True)
    require(archive.stat().st_size <= MAX_EIF_BYTES + MAX_SMALL_FILE_BYTES, "Downloaded artifact is too large")
    extract_candidate(archive, candidate)
    archive.unlink()


def expected_statement(candidate, release):
    """Recompute the statement from the recorded candidate and re-check the files it names."""
    recorded = read_json(release / "candidate.json")
    digests = check_checksums(candidate)
    require(digests["image.eif"] == recorded["eifSha256"], "The approved EIF changed after verification")
    statement = statement_bytes(recorded["environment"], recorded["sourceCommit"], recorded["measurements"],
                                recorded["buildRunId"], recorded["buildRunAttempt"], recorded["eifSha256"])
    require((release / "statement.json").read_bytes() == statement, "The prepared statement was modified")
    return recorded, statement


# --- Bundle profile ---------------------------------------------------------


def pae(payload_type, payload):
    kind = payload_type.encode()
    return b"DSSEv1 %d %s %d %s" % (len(kind), kind, len(payload), payload)


def signing_services():
    """The one Rekor v2 log and one timestamp authority the pinned configuration names."""
    config = read_json(SIGNING_CONFIG)
    require(config.get("mediaType") == "application/vnd.dev.sigstore.signingconfig.v0.2+json",
            "Unexpected signing configuration format")
    require(set(config) == {"mediaType", "rekorTlogUrls", "rekorTlogConfig", "tsaUrls", "tsaConfig"},
            "Signing configuration must name only a transparency log and a timestamp authority")
    logs, tsas = config["rekorTlogUrls"], config["tsaUrls"]
    require(len(logs) == 1 and logs[0].get("majorApiVersion") == 2, "Signing configuration must name one Rekor v2 log")
    require(len(tsas) == 1 and tsas[0].get("majorApiVersion") == 1, "Signing configuration must name one TSA")
    require(config["rekorTlogConfig"] == {"selector": "EXACT", "count": 1}, "Log selector must be EXACT:1")
    require(config["tsaConfig"] == {"selector": "EXACT", "count": 1}, "TSA selector must be EXACT:1")
    root = read_json(TRUSTED_ROOT, MAX_BUNDLE_BYTES)
    log_url = logs[0]["url"]
    trusted = [log for log in root.get("tlogs", []) if log.get("baseUrl") == log_url]
    require(len(trusted) == 1 and trusted[0]["publicKey"].get("keyDetails") == "PKIX_ED25519",
            "Trusted root must hold exactly one Ed25519 key for the configured Rekor v2 log")
    require(any(authority.get("uri") == tsas[0]["url"] for authority in root.get("timestampAuthorities", [])),
            "Trusted root lacks the configured timestamp authority")
    return {"log_url": log_url, "log_origin": urllib.parse.urlsplit(log_url).hostname,
            "log_key_id": b64decode(trusted[0]["logId"]["keyId"], "log ID")}


def check_checkpoint(envelope, origin, tree_size, root_hash, key_id):
    """Signed-note checkpoint for this tree, with a signature line from the log's own key."""
    require(isinstance(envelope, str), "Missing signed checkpoint")
    body, separator, signatures = envelope.partition("\n\n")
    require(separator, "Checkpoint has no signature block")
    lines = body.split("\n")
    require(len(lines) >= 3 and lines[0] == origin, "Checkpoint is not from the configured log")
    require(lines[1] == str(tree_size) and b64decode(lines[2], "checkpoint root") == root_hash,
            "Checkpoint does not match the inclusion proof")
    # Witness cosignatures may follow the log's own line; only the log's is required here.
    own = [line for line in signatures.splitlines() if line.startswith(f"\u2014 {origin} ")]
    key_hash = key_id[:NOTE_KEY_HASH_BYTES]
    require(any(b64decode(line.rsplit(" ", 1)[1], "checkpoint signature")[:NOTE_KEY_HASH_BYTES] == key_hash
                for line in own),
            "Checkpoint carries no signature from the configured log key")


def check_bundle_profile(data, statement, public_der):
    """Enforce Maple's release bundle profile; Cosign separately verifies the cryptography.

    v0.3 bundle, managed public key, one DSSE in-toto envelope over exactly the
    prepared statement, exactly one Rekor v2 hashedrekord 0.0.2 entry from the
    configured log with an inclusion proof and signed checkpoint, no integrated
    time or inclusion promise, and exactly one RFC 3161 timestamp over this signature.
    """
    services = signing_services()
    require(len(data) <= MAX_BUNDLE_BYTES, "Bundle exceeds its size limit")
    bundle = strict_json(data)
    require(isinstance(bundle, dict) and set(bundle) == {"mediaType", "verificationMaterial", "dsseEnvelope"},
            "Bundle must hold only a media type, verification material and one DSSE envelope")
    require(bundle["mediaType"] == BUNDLE_MEDIA_TYPE, "Bundle is not a v0.3 bundle")
    material = bundle["verificationMaterial"]
    require(isinstance(material, dict) and set(material) == {"publicKey", "tlogEntries", "timestampVerificationData"},
            "Verification material must be a public key, one log entry and one timestamp")
    require(isinstance(material["publicKey"], dict) and set(material["publicKey"]) <= {"hint"},
            "Verification material must identify a managed key, not a certificate")

    payload, signature = check_envelope(bundle["dsseEnvelope"], statement)
    entry = check_log_entry(material["tlogEntries"], services)
    index, tree_size = check_inclusion_proof(entry, services)
    check_logged_body(entry, payload, signature, public_der)
    tokens = check_timestamps(material["timestampVerificationData"], signature)
    return {"logIndex": int(index), "treeSize": int(tree_size), "log": services["log_url"],
            "timestamps": len(tokens)}


def check_envelope(envelope, statement):
    """One in-toto DSSE envelope over exactly the prepared statement, with one signature."""
    require(isinstance(envelope, dict) and set(envelope) == {"payload", "payloadType", "signatures"},
            "Envelope must hold only a payload, its type and signatures")
    require(envelope["payloadType"] == PAYLOAD_TYPE, "Envelope is not in-toto")
    payload = b64decode(envelope.get("payload"), "DSSE payload")
    require(payload == statement, "Signed statement differs from the prepared statement")
    signatures = envelope.get("signatures")
    require(isinstance(signatures, list) and len(signatures) == 1, "Envelope must carry exactly one signature")
    require(isinstance(signatures[0], dict) and "sig" in signatures[0] and set(signatures[0]) <= {"sig", "keyid"},
            "Envelope signature has unexpected fields")
    signature = b64decode(signatures[0].get("sig"), "DSSE signature")
    return payload, signature


def check_log_entry(entries, services):
    """Exactly one Rekor v2 hashedrekord 0.0.2 entry from the configured log."""
    require(isinstance(entries, list) and len(entries) == 1, "Bundle must carry exactly one transparency log entry")
    entry = entries[0]
    require(isinstance(entry, dict), "Log entry is not a JSON object")
    require(entry.get("kindVersion") == {"kind": "hashedrekord", "version": "0.0.2"},
            "Log entry is not a Rekor v2 hashedrekord 0.0.2 entry")
    log_id = json_object(entry.get("logId"), "Log entry log ID")
    require(b64decode(log_id.get("keyId"), "log ID") == services["log_key_id"],
            "Log entry is not from the configured Rekor v2 log")
    # Absence, not a zero value: a Rekor v2 entry carries no integrated time or promise.
    require("integratedTime" not in entry, "Log entry carries an integrated time")
    require("inclusionPromise" not in entry, "Log entry carries an inclusion promise")
    require(set(entry) == {"logIndex", "logId", "kindVersion", "inclusionProof", "canonicalizedBody"},
            "Log entry has missing or unexpected fields")
    return entry


def check_inclusion_proof(entry, services):
    """A Merkle inclusion proof for this entry and a checkpoint signed by the log. Returns index and tree size."""
    index = entry.get("logIndex")
    require(isinstance(index, str) and index.isdigit(), "Log entry has no index")
    proof = entry.get("inclusionProof")
    require(isinstance(proof, dict) and set(proof) == {"logIndex", "rootHash", "treeSize", "hashes", "checkpoint"},
            "Inclusion proof has missing or unexpected fields")
    require(isinstance(proof["checkpoint"], dict) and set(proof["checkpoint"]) == {"envelope"},
            "Checkpoint has unexpected fields")
    require(proof.get("logIndex") == index, "Inclusion proof is for another entry")
    tree_size = proof.get("treeSize")
    require(isinstance(tree_size, str) and tree_size.isdigit() and int(tree_size) > int(index),
            "Inclusion proof has no valid tree size")
    root_hash = b64decode(proof.get("rootHash"), "inclusion proof root")
    require(len(root_hash) == 32, "Inclusion proof root is not SHA-256")
    hashes = proof.get("hashes")
    require(isinstance(hashes, list) and all(len(b64decode(item, "proof hash")) == 32 for item in hashes),
            "Inclusion proof hashes are malformed")
    checkpoint = (proof.get("checkpoint") or {}).get("envelope")
    check_checkpoint(checkpoint, services["log_origin"], tree_size, root_hash, services["log_key_id"])
    return index, tree_size


def check_logged_body(entry, payload, signature, public_der):
    """The logged hashedrekord binds this envelope, its signature and the reviewed key."""
    body = strict_json(b64decode(entry.get("canonicalizedBody"), "log entry body"))
    require(isinstance(body, dict) and body.get("kind") == "hashedrekord" and body.get("apiVersion") == "0.0.2",
            "Log body is not hashedrekord 0.0.2")
    # Each part is checked where it is first read, so the first failing check
    # is the same one as for any other malformed body.
    spec = json_object(json_object(body.get("spec"), "Log body spec").get("hashedRekordV002"),
                       "Log body hashedrekord spec")
    logged_data = json_object(spec.get("data"), "Log body data")
    require(logged_data.get("algorithm") == "SHA2_384", "Log body digest is not SHA-384")
    envelope_digest = hashlib.sha384(pae(PAYLOAD_TYPE, payload)).digest()
    require(b64decode(logged_data.get("digest"), "logged digest") == envelope_digest,
            "Log body digest does not bind this envelope")
    logged_signature = json_object(spec.get("signature"), "Log body signature")
    require(b64decode(logged_signature.get("content"), "logged signature") == signature,
            "Log body signature differs from the envelope signature")
    verifier = json_object(logged_signature.get("verifier"), "Log body verifier")
    require(verifier.get("keyDetails") == "PKIX_ECDSA_P384_SHA_384", "Logged key is not P-384")
    logged_key = json_object(verifier.get("publicKey"), "Log body public key")
    require(b64decode(logged_key.get("rawBytes"), "logged key") == public_der,
            "Logged key is not the reviewed signing key")


def check_timestamps(timestamps, signature):
    """Exactly one RFC 3161 timestamp, whose message imprint is this signature."""
    require(isinstance(timestamps, dict) and set(timestamps) == {"rfc3161Timestamps"},
            "Timestamp data must be RFC 3161 timestamps only")
    tokens = timestamps["rfc3161Timestamps"]
    require(isinstance(tokens, list) and len(tokens) == 1, "Bundle must carry exactly one RFC 3161 timestamp")
    require(isinstance(tokens[0], dict) and set(tokens[0]) == {"signedTimestamp"}, "Timestamp has unexpected fields")
    token = b64decode(tokens[0]["signedTimestamp"], "RFC 3161 timestamp")
    # A byte scan, not verification (Cosign verifies the token): the message
    # imprint is an OCTET STRING holding SHA-256 of the signature.
    require(b"\x04\x20" + hashlib.sha256(signature).digest() in token, "Timestamp does not cover this signature")
    return tokens


def cosign_verify(cosign, bundle, key_path, eif):
    subprocess.run([
        str(cosign), "verify-blob-attestation", "--bundle", str(bundle), "--key", str(key_path),
        "--type", PREDICATE_TYPE, "--use-signed-timestamps", "--trusted-root", str(TRUSTED_ROOT), str(eif),
    ], check=True, env=clean_env())


def verify_bundle(bundle, statement, key_path, public_der, eif, cosign):
    summary = check_bundle_profile(bundle.read_bytes(), statement, public_der)
    cosign_verify(cosign, bundle, key_path, eif)
    return summary


# --- Cosign and signing -----------------------------------------------------


def cosign_platform():
    machine = platform.machine().lower()
    require(sys.platform.startswith("linux") and machine in ("x86_64", "aarch64"),
            "The pinned Cosign build is available for Linux x86_64 and ARM64 only")
    return "linux-amd64" if machine == "x86_64" else "linux-arm64"


def install_cosign(destination):
    name = cosign_platform()
    destination.mkdir(parents=True, exist_ok=True)
    partial = destination / "cosign.partial"
    url = COSIGN_URL.format(version=COSIGN_VERSION, platform=name)
    with urllib.request.urlopen(url, timeout=120) as response, partial.open("xb") as out:
        while chunk := response.read(1024 * 1024):
            require(out.tell() + len(chunk) <= MAX_COSIGN_BYTES, "Cosign download is too large")
            out.write(chunk)
    if sha256_file(partial) != COSIGN_SHA256[name]:
        partial.unlink()
        raise EvidenceError(f"Cosign {COSIGN_VERSION} digest mismatch")
    partial.chmod(0o755)
    partial.rename(destination / "cosign")
    print(f"Installed Cosign {COSIGN_VERSION} ({name}, SHA-256 {COSIGN_SHA256[name]}).")


def check_cosign(cosign):
    require(cosign.is_file() and sha256_file(cosign) == COSIGN_SHA256[cosign_platform()],
            f"Cosign is not the pinned {COSIGN_VERSION} release")


def github_oidc_token(audience="sts.amazonaws.com"):
    url = os.environ.get("ACTIONS_ID_TOKEN_REQUEST_URL")
    token = os.environ.get("ACTIONS_ID_TOKEN_REQUEST_TOKEN")
    require(url and token, "No GitHub OIDC token available; the job needs id-token: write")
    separator = "&" if urllib.parse.urlsplit(url).query else "?"
    request = urllib.request.Request(f"{url}{separator}audience={urllib.parse.quote(audience)}",
                                     headers={"Authorization": f"Bearer {token}"})
    with urllib.request.urlopen(request, timeout=30) as response:
        value = strict_json(response.read(MAX_SMALL_FILE_BYTES)).get("value")
    require(isinstance(value, str) and value.count(".") == 2, "GitHub returned no OIDC token")
    return value


def attest_with_kms(cosign, mode, *, key_arn, role_arn, public_der, statement, eif, bundle):
    """Sign the statement with the KMS key as the signer role, assumed with this job's OIDC token.

    The token file and Cosign's TUF cache live in a private directory removed
    afterwards, and only Cosign receives the role's environment.
    """
    work = Path(tempfile.mkdtemp(prefix="release-statement-"))
    try:
        token = work / "oidc-token"
        token.touch(mode=0o600)
        token.write_text(github_oidc_token())
        session = f"opensecret-{mode}-{os.environ.get('GITHUB_RUN_ID', 'local')}"
        signer_env = clean_env(AWS_ROLE_ARN=role_arn, AWS_WEB_IDENTITY_TOKEN_FILE=str(token),
                               AWS_ROLE_SESSION_NAME=session, AWS_REGION=KMS_REGION, AWS_DEFAULT_REGION=KMS_REGION,
                               TUF_ROOT=str(work / "tuf"))
        key_uri = f"awskms:///{key_arn}"
        # Refuse before anything reaches the public log if KMS holds a different key.
        kms_public_key = subprocess.run([str(cosign), "public-key", "--key", key_uri], env=signer_env, check=True,
                                        stdout=subprocess.PIPE, text=True).stdout
        require(public_key_der(kms_public_key) == public_der,
                "KMS key differs from the reviewed public key; not signing")
        subprocess.run([
            str(cosign), "attest-blob", "--key", key_uri, "--statement", str(statement),
            "--signing-config", str(SIGNING_CONFIG), "--trusted-root", str(TRUSTED_ROOT),
            "--bundle", str(bundle), "--yes", str(eif),
        ], env=signer_env, check=True)
    finally:
        shutil.rmtree(work, ignore_errors=True)


def sign(mode, candidate, release, cosign):
    target = target_for(mode)
    key_arn, role_arn, key_path, public_der = signing_identity(target, os.environ)
    check_cosign(cosign)
    recorded, statement = expected_statement(candidate, release)
    require(recorded["mode"] == mode and recorded["environment"] == target.environment,
            "Prepared candidate is for another environment")
    bundle = release / "bundle.json"
    require(not bundle.exists(), "A bundle already exists for this run")
    attest_with_kms(cosign, mode, key_arn=key_arn, role_arn=role_arn, public_der=public_der,
                    statement=release / "statement.json", eif=candidate / "image.eif", bundle=bundle)
    require(bundle.is_file(), "Cosign wrote no bundle")
    summary = verify_bundle(bundle, statement, key_path, public_der, candidate / "image.eif", cosign)
    write_json(release / "bundle.verification.json", {
        **summary,
        "bundleSha256": sha256_file(bundle),
        "cosign": COSIGN_VERSION,
        "signingConfigSha256": sha256_file(SIGNING_CONFIG),
        "trustedRootSha256": sha256_file(TRUSTED_ROOT),
        "keyArn": key_arn,
        "publicKeySha256": hashlib.sha256(public_der).hexdigest(),
    })
    print(f"Signed and verified the {target.environment} release statement: log index {summary['logIndex']}, "
          f"bundle SHA-256 {sha256_file(bundle)}.")


# --- Publication ------------------------------------------------------------


class NoRedirect(urllib.request.HTTPRedirectHandler):
    """Treat a redirect as a failed read: only the environment's own host may answer."""

    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def public_get(url):
    request = urllib.request.Request(url, headers={
        "Origin": READBACK_ORIGIN, "User-Agent": "maple-release-statement-readback/1", "Accept": "application/json"})
    with urllib.request.build_opener(NoRedirect).open(request, timeout=30) as response:
        require(response.status == 200, f"Public bundle returned HTTP {response.status}")
        require(response.headers.get_content_type() == "application/json", "Public bundle is not served as JSON")
        require(response.headers.get("Access-Control-Allow-Origin") in ("*", READBACK_ORIGIN),
                "Public bundle does not allow browser reads")
        require(response.headers.get("Cache-Control") == CACHE_CONTROL, "Public bundle has the wrong cache policy")
        body = response.read(MAX_BUNDLE_BYTES + 1)
    require(len(body) <= MAX_BUNDLE_BYTES, "Public bundle exceeds its size limit")
    return body


def upload(target, bundle, key):
    account = os.environ.get("ATTESTATIONS_R2_ACCOUNT_ID", "")
    require(is_hex(account, 32), "ATTESTATIONS_R2_ACCOUNT_ID must be a Cloudflare account ID")
    require(os.environ.get("ATTESTATIONS_R2_BUCKET") == target.bucket,
            f"This workflow publishes only to {target.bucket}")
    access_key, secret_key = os.environ.get("AWS_ACCESS_KEY_ID"), os.environ.get("AWS_SECRET_ACCESS_KEY")
    require(access_key and secret_key, "Missing the bucket publisher credential")
    # The CLI adds its own checksums only where an operation requires them;
    # Content-MD5 lets R2 check the uploaded body.
    md5 = base64.b64encode(hashlib.md5(bundle.read_bytes(), usedforsecurity=False).digest()).decode()
    # One PUT of one key: an identical rerun or approved same-PCR replacement
    # overwrites only this object, and nothing is ever deleted.
    result = subprocess.run([
        "aws", "s3api", "put-object", "--endpoint-url", f"https://{account}.r2.cloudflarestorage.com",
        "--region", "auto", "--bucket", target.bucket, "--key", key, "--body", str(bundle),
        "--content-type", "application/json", "--cache-control", CACHE_CONTROL, "--content-md5", md5,
    ], env=clean_env(AWS_ACCESS_KEY_ID=access_key, AWS_SECRET_ACCESS_KEY=secret_key, AWS_PAGER="",
                     AWS_REQUEST_CHECKSUM_CALCULATION="when_required",
                     AWS_RESPONSE_CHECKSUM_VALIDATION="when_required"),
        stdout=subprocess.DEVNULL)
    require(result.returncode == 0, f"Upload of {key} to {target.bucket} failed; nothing was published")


def read_back(url, expected):
    """Fetch the public bundle until it serves exactly the uploaded bytes.

    An approved same-PCR replacement can be hidden by a cached copy for one
    cache lifetime; retry past it, but never report stale bytes as published.
    """
    last_error = None
    for attempt in range(READBACK_ATTEMPTS):
        if attempt:
            time.sleep(READBACK_DELAY)
        try:
            body = public_get(url)
            require(body == expected, "Public bytes differ from the uploaded bundle")
            return body
        except (OSError, EvidenceError) as error:
            last_error = error
    raise EvidenceError(f"Public bundle at {url} was not confirmed: {last_error}")


def publish(mode, candidate, release, cosign, base_url=None):
    target = target_for(mode)
    key_path, public_der = reviewed_public_key(os.environ)
    check_cosign(cosign)
    recorded, statement = expected_statement(candidate, release)
    require(recorded["mode"] == mode, "Prepared candidate is for another environment")
    bundle = release / "bundle.json"
    verify_bundle(bundle, statement, key_path, public_der, candidate / "image.eif", cosign)
    pcr0 = recorded["measurements"]["PCR0"]
    require(is_hex(pcr0, 96), "Invalid PCR0 for the object key")
    key = f"{pcr0}.json"
    upload(target, bundle, key)
    url = f"{base_url or 'https://' + target.host}/{key}"
    public = release / "public.bundle.json"
    public.write_bytes(read_back(url, bundle.read_bytes()))
    summary = verify_bundle(public, statement, key_path, public_der, candidate / "image.eif", cosign)
    receipt = {
        "url": url,
        "bundleSha256": sha256_file(public),
        "eifSha256": recorded["eifSha256"],
        "sourceCommit": recorded["sourceCommit"],
        "environment": target.environment,
        "pcr0": pcr0,
        "logIndex": summary["logIndex"],
    }
    write_json(release / "publication.json", receipt)
    step_summary(f"## Published {target.environment} release bundle\n\n```json\n{json.dumps(receipt, indent=2)}\n```\n")
    print(json.dumps(receipt))


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("preflight").add_argument("mode")
    prepare_parser = commands.add_parser("prepare")
    prepare_parser.add_argument("mode")
    prepare_parser.add_argument("run_id")
    prepare_parser.add_argument("candidate", type=Path)
    prepare_parser.add_argument("release", type=Path)
    commands.add_parser("install-cosign").add_argument("destination", type=Path)
    for name in ("sign", "publish"):
        command = commands.add_parser(name)
        command.add_argument("mode")
        for path in ("candidate", "release", "cosign"):
            command.add_argument(path, type=Path)
    args = parser.parse_args(argv)
    try:
        if args.command == "preflight":
            preflight(args.mode)
        elif args.command == "prepare":
            prepare(args.mode, args.run_id, args.candidate, args.release)
        elif args.command == "install-cosign":
            install_cosign(args.destination)
        elif args.command == "sign":
            sign(args.mode, args.candidate.resolve(), args.release.resolve(), args.cosign.resolve())
        else:
            publish(args.mode, args.candidate.resolve(), args.release.resolve(), args.cosign.resolve())
    except EvidenceError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    except subprocess.CalledProcessError as error:
        print(f"error: {Path(str(error.cmd[0])).name} {error.cmd[1]} exited with {error.returncode}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
