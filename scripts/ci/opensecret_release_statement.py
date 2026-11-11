"""Dev release producer: verify build evidence, sign via actions/attest, publish.

Cryptography belongs to gh/Sigstore. Only gh's verified statements are inspected;
neither decoded DSSE payloads nor caller-supplied measurements grant approval.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import stat
import subprocess
import time
import urllib.error
import urllib.request
import zipfile

REPOSITORY = "MaplePrivacyLabs/Maple"
SOURCE_REPOSITORY = f"https://github.com/{REPOSITORY}"
BUILD_WORKFLOW = ".github/workflows/opensecret-eif-release.yml"
SIGN_WORKFLOW = ".github/workflows/opensecret-release-statement-dev.yml"
PREDICATE_TYPE = "https://maple.ai/attestations/enclave-release/v1"
SLSA_TYPE = "https://slsa.dev/provenance/v1"
ISSUER = "https://token.actions.githubusercontent.com"
PUBLIC_HOST = "https://attestations-dev.maple.ai"
BUCKET = "maple-attestations-dev"
CACHE_CONTROL = "public,max-age=300"
FILES = {"image.eif", "pcr.json", "handoff.json", "SHA256SUMS"}
MAX_BUNDLE = 4 * 1024 * 1024
MAX_EIF = 2 * 1024 * 1024 * 1024


def require(condition, message):
    if not condition:
        raise ValueError(message)


def hex_value(value, length):
    return isinstance(value, str) and re.fullmatch(f"[0-9a-f]{{{length}}}", value) is not None


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def read_json(path):
    def unique_pairs(pairs):
        result = {}
        for key, value in pairs:
            require(key not in result, "Duplicate JSON key")
            result[key] = value
        return result

    require(path.is_file() and not path.is_symlink(), "Expected a regular JSON file")
    require(path.stat().st_size <= MAX_BUNDLE, "JSON evidence exceeds size limit")
    return json.loads(path.read_bytes(), object_pairs_hook=unique_pairs)


def write_json(path, value):
    path.write_text(json.dumps(value, indent=2) + "\n")


def command_json(arguments):
    return json.loads(subprocess.run(arguments, check=True, stdout=subprocess.PIPE, text=True).stdout)


def api(path):
    return command_json(["gh", "api", f"repos/{REPOSITORY}/{path}"])


def validate_run(run, run_id):
    require(str(run.get("id")) == run_id, "Build run ID mismatch")
    require(run.get("repository", {}).get("full_name") == REPOSITORY,
            "Build must belong to the canonical repository")
    require(run.get("head_repository", {}).get("full_name") == REPOSITORY,
            "Fork builds cannot be approved")
    require(run["repository"].get("private") is False, "Public Sigstore requires the public repository")
    require(run.get("path") == BUILD_WORKFLOW, "Unexpected build workflow")
    require(run.get("event") == "workflow_dispatch" and run.get("head_branch") == "master",
            "Only manually selected master builds can be approved")
    require(run.get("status") == "completed" and run.get("conclusion") == "success",
            "Build run must have completed successfully")
    require(hex_value(run.get("head_sha"), 40), "Invalid full build source SHA")
    require(type(run.get("run_attempt")) is int and run["run_attempt"] > 0, "Invalid build attempt")


def extract_candidate(archive_path, destination):
    # Do not let an archive choose paths, executable files, symlinks or size.
    destination.mkdir()
    with zipfile.ZipFile(archive_path) as archive:
        entries = archive.infolist()
        require(len(entries) == len(FILES) and {item.filename for item in entries} == FILES,
                "Candidate archive must contain exactly the four build files")
        for entry in entries:
            file_type = stat.S_IFMT(entry.external_attr >> 16)
            require(file_type in (0, stat.S_IFREG), "Candidate contains a non-regular file")
            limit = MAX_EIF if entry.filename == "image.eif" else 64 * 1024
            require(0 < entry.file_size <= limit, "Candidate file exceeds size limit or is empty")
            with archive.open(entry) as source, (destination / entry.filename).open("xb") as target:
                shutil.copyfileobj(source, target)


def verify(path, workflow, source_sha, predicate_type, bundle=None):
    arguments = [
        "gh", "attestation", "verify", str(path), "--repo", REPOSITORY,
        "--cert-identity", f"{SOURCE_REPOSITORY}/{workflow}@refs/heads/master",
        "--cert-oidc-issuer", ISSUER, "--source-ref", "refs/heads/master",
        "--source-digest", source_sha, "--signer-digest", source_sha,
        "--predicate-type", predicate_type, "--format", "json",
    ]
    if bundle is not None:
        arguments += ["--bundle", str(bundle), "--deny-self-hosted-runners"]
    results = command_json(arguments)
    require(isinstance(results, list) and results, "Verifier returned no authenticated statements")
    return results


def statement(result):
    return result.get("verificationResult", {}).get("statement", {})


def has_subject(value, path):
    return any(subject.get("digest") == {"sha256": digest(path)}
               for subject in value.get("subject", []))


def validate_candidate(candidate, run):
    checksums = (candidate / "SHA256SUMS").read_text().splitlines()
    expected = {f"{digest(candidate / name)}  {name}" for name in FILES - {"SHA256SUMS"}}
    require(len(checksums) == 3 and set(checksums) == expected, "Candidate checksum mismatch")
    handoff = read_json(candidate / "handoff.json")
    measurements = read_json(candidate / "pcr.json")
    require(handoff.get("environment") == "dev", "Candidate was not built for Dev")
    require(handoff.get("source_sha") == run["head_sha"], "Candidate source differs from the selected run")
    require(handoff.get("eif_sha256") == digest(candidate / "image.eif"), "EIF differs from handoff")
    require(handoff.get("measurements") == measurements, "Measurements differ from handoff")
    require(isinstance(measurements.get("HashAlgorithm"), str), "Missing measurement algorithm")
    for name in ("PCR0", "PCR1", "PCR2"):
        require(hex_value(measurements.get(name), 96) and measurements[name] != "0" * 96,
                f"Invalid {name}")
    return {
        "environment": "development",
        "source": {"repository": SOURCE_REPOSITORY, "commit": run["head_sha"]},
        "measurements": {name: measurements[name] for name in ("PCR0", "PCR1", "PCR2")},
        "build": {"runId": str(run["id"]), "runAttempt": run["run_attempt"]},
    }


def prepare(run_id, candidate, release):
    require(re.fullmatch(r"[1-9][0-9]{0,19}", run_id), "Expected a numeric build run ID")
    require(not candidate.exists() and not release.exists(), "Output directories must be new")
    run = api(f"actions/runs/{run_id}")
    validate_run(run, run_id)
    # The selected build may predate this workflow, but must be in master history.
    subprocess.run(["git", "merge-base", "--is-ancestor", run["head_sha"], "HEAD"], check=True)
    artifacts = command_json([
        "gh", "api", "--paginate", "--slurp",
        f"repos/{REPOSITORY}/actions/runs/{run_id}/artifacts?per_page=100",
    ])
    name = f"opensecret-eif-dev-{run['head_sha'][:12]}"
    selected = [artifact for page in artifacts for artifact in page["artifacts"]
                if artifact.get("name") == name and artifact.get("expired") is False]
    require(len(selected) == 1, "Expected exactly one retained Dev artifact for this source")
    artifact = selected[0]
    require(type(artifact.get("id")) is int and artifact["id"] > 0, "Invalid artifact ID")
    require(artifact.get("workflow_run", {}).get("id") == run["id"], "Artifact run mismatch")
    require(0 < artifact.get("size_in_bytes", 0) <= MAX_EIF, "Artifact exceeds size limit")
    release.mkdir()
    archive = release / "candidate.zip"
    with archive.open("xb") as output:
        subprocess.run(["gh", "api", f"repos/{REPOSITORY}/actions/artifacts/{artifact['id']}/zip"],
                       stdout=output, check=True)
    extract_candidate(archive, candidate)
    archive.unlink()
    invocation = f"{SOURCE_REPOSITORY}/actions/runs/{run_id}/attempts/{run['run_attempt']}"
    for name in ("image.eif", "pcr.json", "handoff.json"):
        path = candidate / name
        evidence = verify(path, BUILD_WORKFLOW, run["head_sha"], SLSA_TYPE)
        require(any(
            statement(item).get("predicateType") == SLSA_TYPE
            and has_subject(statement(item), path)
            and statement(item).get("predicate", {}).get("runDetails", {}).get("metadata", {}).get(
                "invocationId") == invocation
            for item in evidence
        ), "Build provenance does not identify the selected run attempt and artifact")
        write_json(release / f"{name}.verification.json", evidence)
    predicate = validate_candidate(candidate, run)
    write_json(release / "predicate.json", predicate)
    write_json(release / "candidate.json", {
        "predicate": predicate, "eifSha256": digest(candidate / "image.eif"),
        "signerCommit": os.environ["GITHUB_SHA"], "artifactId": artifact["id"],
    })
    if os.environ.get("GITHUB_OUTPUT"):
        with open(os.environ["GITHUB_OUTPUT"], "a") as output:
            output.write(f"source_sha={run['head_sha']}\neif_sha256={digest(candidate / 'image.eif')}\n")


def verify_release(candidate, release, bundle):
    read_json(bundle)  # Bound size and reject ambiguous JSON before handing it to gh.
    expected = read_json(release / "candidate.json")
    require(digest(candidate / "image.eif") == expected["eifSha256"], "Approved EIF was changed")
    results = verify(candidate / "image.eif", SIGN_WORKFLOW, expected["signerCommit"], PREDICATE_TYPE, bundle)
    require(len(results) == 1, "Expected one release statement")
    value = statement(results[0])
    require(value.get("_type") == "https://in-toto.io/Statement/v1"
            and value.get("predicateType") == PREDICATE_TYPE
            and value.get("predicate") == expected["predicate"]
            and has_subject(value, candidate / "image.eif"), "Release statement differs from approved candidate")
    return results


def retain(candidate, release, bundle):
    verified = verify_release(candidate, release, bundle)
    shutil.copyfile(bundle, release / "bundle.json")
    write_json(release / "bundle.verification.json", verified)


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def public_request(url, method):
    request = urllib.request.Request(url, method=method, headers={"Origin": "https://trymaple.ai"})
    with urllib.request.build_opener(NoRedirect).open(request, timeout=30) as response:
        require(response.status == 200, "Public bundle request did not return 200")
        require(response.headers.get_content_type() == "application/json", "Public bundle is not JSON")
        require(response.headers.get("Access-Control-Allow-Origin") == "*", "Public bundle lacks browser CORS")
        require(response.headers.get("Cache-Control") == CACHE_CONTROL, "Public bundle cache policy differs")
        body = response.read(MAX_BUNDLE + 1) if method == "GET" else b""
        require(len(body) <= MAX_BUNDLE, "Public bundle exceeds size limit")
        return body


def publish(candidate, release):
    bundle = release / "bundle.json"
    verify_release(candidate, release, bundle)
    account = os.environ.get("ATTESTATIONS_R2_ACCOUNT_ID", "")
    require(hex_value(account, 32), "Expected a Cloudflare account ID")
    require(os.environ.get("ATTESTATIONS_R2_BUCKET") == BUCKET, "Dev publisher must use the Dev bucket")
    expected = read_json(release / "candidate.json")
    key = expected["predicate"]["measurements"]["PCR0"] + ".json"
    require(re.fullmatch(r"[0-9a-f]{96}\.json", key), "Invalid public object key")
    subprocess.run([
        "aws", "s3api", "put-object", "--endpoint-url", f"https://{account}.r2.cloudflarestorage.com",
        "--region", "auto", "--bucket", BUCKET, "--key", key, "--body", str(bundle),
        "--content-type", "application/json", "--cache-control", CACHE_CONTROL,
    ], check=True, stdout=subprocess.DEVNULL)
    url = f"{PUBLIC_HOST}/{key}"
    public_bundle = release / "public.bundle.json"
    # Allow a complete 300s cache lifetime for same-PCR replacements; never
    # report a stale lookup file as a successfully published approval.
    for attempt in range(12):
        try:
            body = public_request(url, "GET")
            require(body == bundle.read_bytes(), "Public bundle bytes differ from the uploaded approval")
            public_request(url, "HEAD")
            public_bundle.write_bytes(body)
            verified = verify_release(candidate, release, public_bundle)
            write_json(release / "public.verification.json", verified)
            break
        except (OSError, ValueError, urllib.error.URLError):
            if attempt == 11:
                raise
            time.sleep(30)
    receipt = {"url": url, "bundleSha256": digest(public_bundle), "eifSha256": expected["eifSha256"],
               "sourceCommit": expected["predicate"]["source"]["commit"]}
    write_json(release / "publication.json", receipt)
    if os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(os.environ["GITHUB_STEP_SUMMARY"], "a") as summary:
            summary.write("## Verified Dev approval publication\n\n```json\n" + json.dumps(receipt, indent=2) + "\n```\n")
    print(json.dumps(receipt))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    prepare_parser = commands.add_parser("prepare")
    prepare_parser.add_argument("run_id")
    subparsers = {"prepare": prepare_parser}
    subparsers.update({name: commands.add_parser(name) for name in ("retain", "publish")})
    for name, subparser in subparsers.items():
        subparser.add_argument("candidate", type=Path)
        subparser.add_argument("release", type=Path)
        if name == "retain":
            subparser.add_argument("bundle", type=Path)
    args = parser.parse_args()
    if args.command == "prepare":
        prepare(args.run_id, args.candidate, args.release)
    elif args.command == "retain":
        retain(args.candidate, args.release, args.bundle)
    else:
        publish(args.candidate, args.release)


if __name__ == "__main__":
    main()
