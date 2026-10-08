#!/usr/bin/env python3
"""Publish a separately authorized, successful master auth build; never app releases."""

from __future__ import annotations

import argparse
from dataclasses import dataclass
from functools import partial
import json
import os
from pathlib import Path
import re
import subprocess
import sys
from urllib.error import HTTPError, URLError

sys.path.insert(0, str(Path(__file__).resolve().parent))
import pages_deploy as pages
from pages_artifact import AUTH_ARCHIVE_NAME, extract_static

BUILD_WORKFLOW = "auth-pages-build.yml"
REPOSITORY_ID = 923138240
AUTH_HEADERS = """/*
  Cache-Control: no-store, max-age=0
  X-Robots-Tag: noindex, nofollow
  Referrer-Policy: no-referrer
  X-Frame-Options: DENY
  Content-Security-Policy: frame-ancestors 'none'
"""


@dataclass(frozen=True)
class AuthTarget:
    build_workflow: str
    profile: str
    artifact_prefix: str
    enabled_variable: str
    destination: pages.Destination


# Fixed trusted destinations: no dispatch input or artifact can name a host,
# environment, ref, project, or alternate workflow.
TARGETS = {
    "production": AuthTarget(
        BUILD_WORKFLOW, "auth-release", "maple-auth-production",
        "MAPLE_AUTH_PAGES_PRODUCTION_ENABLED",
        pages.Destination("maple-auth", "maple-auth.pages.dev",
                          "auth-pages-production", "auth-pages-production",
                          "https://auth.maple.ai", AUTH_HEADERS, allow_direct_upload=True)),
    "development": AuthTarget(
        "auth-pages-dev-build.yml", "auth-dev", "maple-auth-development",
        "MAPLE_AUTH_PAGES_DEVELOPMENT_ENABLED",
        pages.Destination("maple-auth-dev", "maple-auth-dev.pages.dev",
                          "auth-pages-development", "auth-pages-development",
                          "https://auth-dev.maple.ai", AUTH_HEADERS,
                          allow_direct_upload=True, github_production=False)),
}
DESTINATION = TARGETS["production"].destination


def auth_target(environment):
    pages.require(environment in TARGETS, "Invalid auth environment")
    return TARGETS[environment]


def input_number(value):
    pages.require(isinstance(value, str) and re.fullmatch(r"[1-9][0-9]{0,19}", value),
                  "Invalid auth build identifier")
    return pages.number(int(value))


def select_plan(gh, event, target="production", *, environment="production"):
    config = auth_target(environment)
    pages.require(target == "production" and "workflow_run" not in event,
                  "Auth publication requires a manual selection")
    repo = gh.get("")
    pages.require(gh.repository_id == REPOSITORY_ID and repo["id"] == REPOSITORY_ID
                  and repo["default_branch"] == "master", "Unexpected auth repository identity")
    inputs = event["inputs"]
    pages.require(set(inputs) == {"build_run_id", "build_run_attempt"}, "Unexpected auth build inputs")
    run_id, attempt = (input_number(inputs[key]) for key in ("build_run_id", "build_run_attempt"))
    run = gh.run(run_id, config.build_workflow, "workflow_dispatch", attempt)
    pages.require(run["id"] == run_id and run["head_branch"] == "master",
                  "Auth build must be dispatched from master")
    if gh.get("/git/ref/heads/master")["object"]["sha"] != run["head_sha"]:
        raise pages.Superseded("Auth build no longer matches master; build the current revision")
    previous_sha = pages.sha(gh.get(f"/git/ref/heads/{config.destination.production_branch}")["object"]["sha"])
    if previous_sha != run["head_sha"]:
        pages.require(gh.get(f"/compare/{previous_sha}...{run['head_sha']}")["status"] == "ahead",
                      "Non-forward auth deployment change")
    artifacts = gh.get(f"/actions/runs/{run_id}/artifacts?per_page=100")
    pages.require(type(artifacts["total_count"]) is int and 0 < artifacts["total_count"] <= 100,
                  "Invalid auth build artifact count")
    artifact_name = f"{config.artifact_prefix}-{run_id}-{attempt}"
    matches = [item for item in artifacts["artifacts"]
               if item["name"] == artifact_name and item["expired"] is False]
    pages.require(len(matches) == 1, "Expected one auth artifact from this build attempt")
    artifact = matches[0]
    pages.require(type(artifact["size_in_bytes"]) is int
                  and 0 < artifact["size_in_bytes"] <= pages.MAX_DOWNLOAD, "Invalid auth artifact size")
    return {"target": "production", "auth_environment": environment,
            "profile": config.profile, "sha": run["head_sha"],
            "branch": config.destination.production_branch, "previous_sha": previous_sha,
            "run_id": run_id, "run_attempt": attempt, "artifact_id": pages.number(artifact["id"]),
            "artifact_digest": pages.digest(artifact["digest"])}


def prepare(gh, event, state, *, environment="production"):
    config = auth_target(environment)
    plan = select_plan(gh, event, environment=environment)
    pages.require(not state.exists() and not state.is_symlink(), "Auth deployment state already exists")
    state.mkdir(mode=0o700, parents=False)
    archive_digest = pages.download_build_artifact(gh, plan, state,
                                                  archive_name=AUTH_ARCHIVE_NAME,
                                                  expected_profile=config.profile)
    files = extract_static(state / "web.tar.gz", state / "assets", archive_digest)
    (state / "plan.json").write_text(json.dumps({"selection": plan, "archive_digest": archive_digest,
                                                "files": files}))
    return plan


def require_publisher_environment(environment="production"):
    config = auth_target(environment)
    pages.require(os.environ.get(config.enabled_variable) == "true",
                  "Auth publication is disabled for the selected environment")
    pages.require(os.environ.get("GITHUB_REF") == "refs/heads/master"
                  and os.environ.get("GITHUB_EVENT_NAME") == "workflow_dispatch",
                  "Auth publisher must be manually dispatched from protected master")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=["prepare", "deploy"])
    parser.add_argument("--state", type=Path, required=True)
    parser.add_argument("--environment", choices=tuple(TARGETS), default="production")
    args = parser.parse_args()
    require_publisher_environment(args.environment)
    config = auth_target(args.environment)
    checkout_sha = subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip()
    pages.require(checkout_sha == pages.sha(os.environ.get("GITHUB_SHA")),
                  "Auth publisher checkout must match the trusted workflow SHA")
    event = json.loads(Path(os.environ["GITHUB_EVENT_PATH"]).read_text())
    gh = pages.GitHub(pages.API("https://api.github.com", os.environ.get("GH_TOKEN", "")),
                      os.environ["GITHUB_REPOSITORY"], int(os.environ["GITHUB_REPOSITORY_ID"]))
    state = args.state.resolve()
    pages.require(state.parent == Path(os.environ["RUNNER_TEMP"]).resolve(),
                  "Auth state must be outside the checkout in runner temp")
    if args.command == "prepare":
        prepare(gh, event, state, environment=args.environment)
    else:
        pages.deploy(gh, event, state, destination=config.destination,
                     selector=partial(select_plan, environment=args.environment))


if __name__ == "__main__":
    try:
        main()
    except (pages.Rejected, ValueError, KeyError, TypeError, OSError,
            HTTPError, URLError, subprocess.SubprocessError):
        print("Auth Pages publisher rejected the operation. Check activation, source, artifact, and destination prerequisites.",
              file=sys.stderr)
        sys.exit(1)
