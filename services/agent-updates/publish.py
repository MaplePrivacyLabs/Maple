#!/usr/bin/env python3
"""Sign this run's Prod Maple Work packages into update feeds and upload them to R2.

Usage: publish.py PACKAGES

PACKAGES holds this workflow run's `maple-agent-prod-<platform>-<run id>`
artifact directories. Environment: AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY,
R2_ENDPOINT, R2_BUCKET, AGENT_UPDATE_SIGNING_KEY (minisign secret key),
RELEASE_NOTES, and GitHub's GITHUB_RUN_ID, GITHUB_RUN_NUMBER and GITHUB_SHA.
"""

from datetime import datetime, timezone
import hashlib
import hmac
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import urllib.parse
import urllib.request

# Installed clients poll one pointer per platform and accept only that pointer's
# manifest format and target names (apps/maple-agent/crates/maple-agent-updater/src/manifest.rs).
# (package platform, format, release envelope, pointer, update target, suffix, download target, suffix)
FEEDS = (
    ("macos-aarch64", "maple-agent-update-v1", "manifest.json", "latest.json",
     "darwin-aarch64", ".app.tar.gz", "macos", ".dmg"),
    ("linux-x86_64", "maple-agent-update-v2", "linux-manifest.json", "linux/latest.json",
     "linux-x86_64", ".tar.gz", "linux-bundle", ".tar.gz"),
    ("windows-x86_64", "maple-agent-update-v3", "windows-manifest.json", "windows/latest.json",
     "windows-x86_64", ".tar.gz", "windows", "-setup.exe"),
    ("windows-aarch64", "maple-agent-update-v4", "windows-arm64-manifest.json", "windows-arm64/latest.json",
     "windows-aarch64", ".tar.gz", "windows-arm64", "-setup.exe"),
)
LIVE_FEED = "https://updates.maple.ai/agent/stable/latest.json"


def encode(value):
    return (json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":")) + "\n").encode()


def sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while block := source.read(1 << 20):
            digest.update(block)
    return digest.hexdigest()


def put(key, body, digest, size):
    """One SigV4-signed S3 PUT to R2."""
    endpoint, bucket = os.environ["R2_ENDPOINT"].rstrip("/"), os.environ["R2_BUCKET"]
    path = f"/{bucket}/" + urllib.parse.quote(key, safe="/-_.~")
    now = datetime.now(timezone.utc)
    timestamp, day = now.strftime("%Y%m%dT%H%M%SZ"), now.strftime("%Y%m%d")
    headers = {"content-length": str(size), "host": urllib.parse.urlsplit(endpoint).netloc,
               "x-amz-content-sha256": digest, "x-amz-date": timestamp}
    names = ";".join(sorted(headers))
    canonical = "PUT\n{}\n\n{}\n{}\n{}".format(
        path, "".join(f"{name}:{headers[name]}\n" for name in sorted(headers)), names, digest)
    scope = f"{day}/auto/s3/aws4_request"
    signing = ("AWS4" + os.environ["AWS_SECRET_ACCESS_KEY"]).encode()
    for part in (day, "auto", "s3", "aws4_request"):
        signing = hmac.new(signing, part.encode(), hashlib.sha256).digest()
    to_sign = f"AWS4-HMAC-SHA256\n{timestamp}\n{scope}\n{hashlib.sha256(canonical.encode()).hexdigest()}"
    signature = hmac.new(signing, to_sign.encode(), hashlib.sha256).hexdigest()
    headers["authorization"] = (f"AWS4-HMAC-SHA256 Credential={os.environ['AWS_ACCESS_KEY_ID']}/{scope}, "
                                f"SignedHeaders={names}, Signature={signature}")
    request = urllib.request.Request(endpoint + path, method="PUT", data=body, headers=headers)
    with urllib.request.urlopen(request, timeout=600):
        pass
    print(f"uploaded {key}")


def put_file(key, path, digest):
    with path.open("rb") as body:
        put(key, body, digest, path.stat().st_size)


def put_bytes(key, data):
    put(key, data, hashlib.sha256(data).hexdigest(), len(data))


def sign(manifest, version, secret_key):
    with tempfile.TemporaryDirectory() as temporary:
        message, signature = Path(temporary, "manifest.json"), Path(temporary, "manifest.json.minisig")
        message.write_bytes(manifest)
        subprocess.run(["minisign", "-S", "-W", "-s", secret_key, "-m", message, "-x", signature,
                        "-c", "Maple Agent update", "-t", f"product=maple-agent channel=stable version={version}"],
                       stdin=subprocess.DEVNULL, check=True)
        return signature.read_text()


def main():
    packages, run_id = Path(sys.argv[1]), os.environ["GITHUB_RUN_ID"]
    directories = {platform: packages / f"maple-agent-prod-{platform}-{run_id}" for platform, *_ in FEEDS}
    version = json.loads((directories["macos-aarch64"] / "build-info.json").read_text())["version"]

    # Published release paths are cached as immutable, so a version is uploaded once.
    request = urllib.request.Request(LIVE_FEED, headers={"X-Maple-Download-Probe": "1"})
    with urllib.request.urlopen(request, timeout=60) as response:
        live = json.loads(json.loads(response.read())["manifest"])["version"]
    if live == version:
        sys.exit(f"Maple Work {version} is already published; bump apps/maple-agent/Cargo.toml and release again")

    release = f"stable/releases/{version}"
    common = {"product": "maple-agent", "channel": "stable", "version": version,
              "build": os.environ["GITHUB_RUN_NUMBER"], "source_sha": os.environ["GITHUB_SHA"],
              "pub_date": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
              "notes": os.environ.get("RELEASE_NOTES", "")}
    payloads, envelopes = {}, []
    with tempfile.TemporaryDirectory() as temporary:
        secret_key = Path(temporary, "update.key")
        secret_key.write_text(os.environ["AGENT_UPDATE_SIGNING_KEY"].strip() + "\n")
        for platform, format_, envelope, pointer, target, suffix, download, download_suffix in FEEDS:
            records = []
            for wanted in (suffix, download_suffix):
                [path] = [path for path in directories[platform].iterdir() if path.name.endswith(wanted)]
                payloads[path.name] = (path, sha256(path))
                records.append({"path": f"/agent/{release}/{path.name}", "size": path.stat().st_size,
                                "sha256": payloads[path.name][1]})
            manifest = encode({**common, "format": format_, "artifacts": {target: records[0]},
                               "downloads": {download: records[1]}})
            signature = sign(manifest, version, str(secret_key))
            envelopes.append((envelope, pointer, encode({"manifest": manifest.decode(), "signature": signature})))

    # Payloads, then each version's envelopes, then the pointers clients poll.
    for name, (path, digest) in payloads.items():
        put_file(f"{release}/{name}", path, digest)
    for envelope, _, data in envelopes:
        put_bytes(f"{release}/{envelope}", data)
    for _, pointer, data in envelopes:
        put_bytes(f"stable/{pointer}", data)
    print(f"published Maple Work {version}")


if __name__ == "__main__":
    main()
