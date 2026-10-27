"""Select one immutable historical signed Windows PE for a read-only probe."""

import hashlib
import io
import json
import os
from pathlib import Path, PurePosixPath
import re
import stat
import subprocess
import threading
import zipfile
from datetime import datetime


REPOSITORY = "MaplePrivacyLabs/Maple"
PRODUCER = ".github/workflows/agent-desktop-build.yml"
JOBS = ("Build Windows binary (prod)", "Signed Windows (prod)",
        "Verify downloaded Windows (prod)")
# The runner each job may report. Signing and verification run on GitHub's
# image; the unsigned build runs on Blacksmith's (on GitHub's before CI moved it).
RUNNERS = ({"windows-2025", "blacksmith-8vcpu-windows-2025", "maple-windows-2025-16core"}, {"windows-2025"}, {"windows-2025"})
MAX_ZIP = 64 * 1024 * 1024


def selection(environment):
    values = {}
    for name in ("RUN", "ATTEMPT", "ARTIFACT", "ZIP_BYTES"):
        value = environment.get("MAPLE_PROBE_" + name, "")
        if not re.fullmatch(r"[1-9][0-9]{0,18}", value):
            raise ValueError("Invalid positive numeric selector: " + name)
        values[name] = int(value)
    if values["ZIP_BYTES"] > MAX_ZIP:
        raise ValueError("Reference ZIP exceeds the fixed 64 MiB bound")
    for name, length in (("SOURCE", 40), ("ZIP_SHA", 64), ("PE_SHA", 64)):
        value = environment.get("MAPLE_PROBE_" + name, "")
        if not re.fullmatch(r"[0-9a-f]{" + str(length) + "}", value):
            raise ValueError("Invalid exact source/digest selector: " + name)
        values[name] = value
    member = environment.get("MAPLE_PROBE_PE_MEMBER", "")
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]{0,159}\.exe", member):
        raise ValueError("Reference PE must be one portable basename")
    values["PE_MEMBER"] = member
    return values


def gh_bytes(path, limit, timeout=45):
    environment = dict(os.environ)
    environment.pop("GH_DEBUG", None)
    process = subprocess.Popen(
        ["gh", "api", "--hostname", "github.com", "repos/" + REPOSITORY + path],
        stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, env=environment,
    )
    timer = threading.Timer(timeout, process.kill)
    timer.daemon = True
    timer.start()
    result = bytearray()
    try:
        while True:
            chunk = process.stdout.read(65536)
            if not chunk:
                break
            if len(result) + len(chunk) > limit:
                raise ValueError("GitHub response exceeded its fixed bound")
            result.extend(chunk)
        if process.wait(timeout=10) != 0:
            raise RuntimeError("Bounded GitHub reference request failed")
        return bytes(result)
    finally:
        timer.cancel()
        if process.poll() is None:
            process.kill()
            process.wait(timeout=10)
        process.stdout.close()


def instant(value):
    return datetime.fromisoformat(value.replace("Z", "+00:00"))


def validate_producer(selected, run, artifact, jobs):
    if (run["id"] != selected["RUN"] or run["run_attempt"] != selected["ATTEMPT"]
            or run["head_sha"] != selected["SOURCE"] or run["head_branch"] != "master"
            or run["event"] not in ("push", "workflow_dispatch") or run["path"] != PRODUCER
            or run["repository"]["full_name"] != REPOSITORY
            or run["head_repository"]["full_name"] != REPOSITORY
            or run["status"] != "completed" or run["conclusion"] != "success"):
        raise ValueError("Reference is not the selected successful trusted master producer")
    if (artifact["id"] != selected["ARTIFACT"] or artifact["expired"]
            or artifact["name"] != "maple-agent-prod-windows-x86_64-" + str(selected["RUN"])
            or artifact["size_in_bytes"] != selected["ZIP_BYTES"]
            or artifact["digest"] != "sha256:" + selected["ZIP_SHA"]
            or artifact["workflow_run"]["id"] != selected["RUN"]
            or artifact["workflow_run"]["head_sha"] != selected["SOURCE"]
            or artifact["workflow_run"]["repository_id"] != run["repository"]["id"]
            or artifact["workflow_run"]["head_repository_id"] != run["repository"]["id"]):
        raise ValueError("Reference artifact identity changed")
    if jobs["total_count"] > 100:
        raise ValueError("Reference jobs exceed the bounded one-page inventory")
    chosen = []
    for name, runners in zip(JOBS, RUNNERS):
        matches = [job for job in jobs["jobs"] if job["name"] == name]
        if (len(matches) != 1 or matches[0]["status"] != "completed"
                or matches[0]["conclusion"] != "success"
                or not runners & set(matches[0]["labels"])):
            raise ValueError("Exact native Prod build/sign/download job did not pass")
        chosen.append(matches[0])
    signing = chosen[1]
    if not (instant(signing["started_at"]) <= instant(artifact["created_at"])
            <= instant(signing["completed_at"])):
        raise ValueError("Artifact does not belong to the selected signing attempt")
    return [{key: job[key] for key in ("id", "name", "conclusion")} for job in chosen]


def reference_pe(selected, archive):
    if len(archive) != selected["ZIP_BYTES"] or hashlib.sha256(archive).hexdigest() != selected["ZIP_SHA"]:
        raise ValueError("Reference ZIP differs from its exact size/digest")
    with zipfile.ZipFile(io.BytesIO(archive)) as payload:
        members = payload.infolist()
        if len(members) > 32 or sum(item.file_size for item in members) > 256 * 1024 * 1024:
            raise ValueError("Reference inventory exceeds fixed bounds")
        names = set()
        for item in members:
            path = PurePosixPath(item.filename)
            if (path.is_absolute() or ".." in path.parts or "\\" in item.filename
                    or ":" in item.filename or item.filename.lower() in names
                    or stat.S_ISLNK(item.external_attr >> 16) or item.flag_bits & 1
                    or item.compress_type not in (zipfile.ZIP_STORED, zipfile.ZIP_DEFLATED)):
                raise ValueError("Unsafe reference ZIP inventory")
            names.add(item.filename.lower())
        for name in ("build-info.json", "manifest.json"):
            if payload.getinfo(name).file_size > 65536:
                raise ValueError("Reference metadata exceeds its bound")
        build = json.loads(payload.read("build-info.json"))
        manifest = json.loads(payload.read("manifest.json"))
        if (build["source_sha"] != selected["SOURCE"] or build["profile"] != "prod"
                or build["bundle_id"] != "cloud.opensecret.maple.agent"
                or manifest["source_sha"] != selected["SOURCE"] or manifest["profile"] != "prod"
                or manifest["platform"] != "windows-x86_64" or manifest["distribution"] != "master"
                or manifest["windows_signing"] != "authenticode"
                or manifest["version"] != build["version"] or manifest["package"] != selected["PE_MEMBER"]):
            raise ValueError("Reference package metadata differs from its selected producer")
        if payload.getinfo(selected["PE_MEMBER"]).file_size > MAX_ZIP:
            raise ValueError("Reference PE exceeds its fixed bound")
        executable = payload.read(selected["PE_MEMBER"])
        if executable[:2] != b"MZ" or hashlib.sha256(executable).hexdigest() != selected["PE_SHA"]:
            raise ValueError("Reference PE differs from its exact digest")
        return executable


def main():
    selected = selection(os.environ)
    report = Path(os.environ["RUNNER_TEMP"]) / "maple-windows-signature-probe"
    report.mkdir(exist_ok=True)
    run = json.loads(gh_bytes("/actions/runs/{}/attempts/{}".format(selected["RUN"], selected["ATTEMPT"]), 1024 * 1024))
    artifact = json.loads(gh_bytes("/actions/artifacts/" + str(selected["ARTIFACT"]), 1024 * 1024))
    jobs = json.loads(gh_bytes("/actions/runs/{}/attempts/{}/jobs?per_page=100".format(selected["RUN"], selected["ATTEMPT"]), 1024 * 1024))
    chosen = validate_producer(selected, run, artifact, jobs)
    archive = gh_bytes("/actions/artifacts/{}/zip".format(selected["ARTIFACT"]), selected["ZIP_BYTES"], 120)
    executable = reference_pe(selected, archive)
    (report / "reference.exe").write_bytes(executable)
    (report / "selection.json").write_text(json.dumps({
        "schema": "maple-windows-historical-signature-selection-v1",
        "probe_source": os.environ["GITHUB_SHA"], "probe_run": os.environ["GITHUB_RUN_ID"],
        "probe_attempt": os.environ["GITHUB_RUN_ATTEMPT"], "repository": REPOSITORY,
        "selection": selected, "producer_jobs": chosen,
        "new_updater_lifecycle_qualified": False, "final_source_release_qualified": False,
    }, indent=2) + "\n")
    print("Exact historical signed PE downloaded and hash-verified; it was not executed.")


if __name__ == "__main__":
    main()
