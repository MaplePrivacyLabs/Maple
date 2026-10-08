#!/usr/bin/env python3
"""Publish verified Auth builds through fixed, independent Dev/Prod destinations."""

from __future__ import annotations

import argparse
from dataclasses import dataclass
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
        "auth-pages-ci.yml", "auth-dev", "maple-auth-development",
        "MAPLE_AUTH_PAGES_DEVELOPMENT_ENABLED",
        pages.Destination("maple-auth-dev", "maple-auth-dev.pages.dev",
                          "auth-pages-development", "auth-pages-development",
                          "https://auth-dev.maple.ai", AUTH_HEADERS,
                          allow_direct_upload=True, github_production=False,
                          preview_environment_prefix="auth-pages",
                          preview_label="Maple Auth development preview",
                          preview_description=("Uses development services. Cloudflare Access applies. "
                                               "Provider OAuth is not configured for PR URLs; "
                                               "use the stable Auth Dev host for OAuth rehearsal."))),
}
DESTINATION = TARGETS["production"].destination


def auth_target(environment):
    pages.require(environment in TARGETS, "Invalid auth environment")
    return TARGETS[environment]


def input_number(value):
    pages.require(isinstance(value, str) and re.fullmatch(r"[1-9][0-9]{0,19}", value),
                  "Invalid auth build identifier")
    return pages.number(int(value))


def select_run(gh, event, config, environment, target):
    if "workflow_run" in event:
        pages.require(environment == "development" and not event.get("inputs"),
                      "Only Dev accepts build-completion events")
        trigger = event["workflow_run"]
        allowed = {"pull_request"} if target == "preview" else {"push", "workflow_dispatch"}
        pages.require(trigger["event"] in allowed, "Auth build event does not match destination")
        run = gh.run(trigger["id"], config.build_workflow, trigger["event"], trigger["run_attempt"])
        pages.require(run["id"] == trigger["id"] and run["head_sha"] == trigger["head_sha"],
                      "Auth trigger identity mismatch")
        return run
    pages.require(target == "production", "Auth previews require a build-completion event")
    inputs = event["inputs"]
    pages.require(set(inputs) == {"build_run_id", "build_run_attempt"}, "Unexpected auth build inputs")
    run_id, attempt = (input_number(inputs[key]) for key in ("build_run_id", "build_run_attempt"))
    # Manual Dev recovery may select either a push or a manually requested build.
    # Production keeps its original dispatch-only build contract.
    run_event = "workflow_dispatch"
    if environment == "development":
        run_event = gh.get(f"/actions/runs/{run_id}")["event"]
        pages.require(run_event in {"push", "workflow_dispatch"}, "Unexpected Dev recovery build")
    run = gh.run(run_id, config.build_workflow, run_event, attempt)
    pages.require(run["id"] == run_id, "Auth build identity mismatch")
    return run


def current_preview_pr(gh, run):
    # Association can be missing from workflow_run.pull_requests. Fetch the
    # commit's PRs and independently verify a unique current internal head.
    # Any same-repository base is valid, including a stacked PR's feature base.
    candidates = gh.get(f"/commits/{run['head_sha']}/pulls?per_page=100")
    pages.require(isinstance(candidates, list) and len(candidates) <= 100,
                  "Invalid Auth preview PR association")
    matches = []
    for candidate in candidates:
        pr = gh.get(f"/pulls/{pages.number(candidate['number'])}")
        if (pr["state"] == "open" and pr["base"].get("repo")
                and pr["base"]["repo"]["id"] == gh.repository_id
                and pr["head"].get("repo") and pr["head"]["repo"]["id"] == gh.repository_id
                and pr["head"]["sha"] == run["head_sha"]
                and pr["head"]["ref"] == run["head_branch"]):
            matches.append(pr)
    if not matches:
        raise pages.Superseded("No current internal PR owns this Auth preview")
    pages.require(len(matches) == 1, "Ambiguous Auth preview PR")
    return pages.number(matches[0]["number"])


def select_plan(gh, event, target="production", *, environment="production"):
    config = auth_target(environment)
    pages.require(target in {"production", "preview"}
                  and (environment == "development" or target == "production"),
                  "Invalid Auth destination kind")
    repo = gh.get("")
    pages.require(gh.repository_id == REPOSITORY_ID and repo["id"] == REPOSITORY_ID
                  and repo["default_branch"] == "master", "Unexpected auth repository identity")
    run = select_run(gh, event, config, environment, target)
    run_id, attempt = pages.number(run["id"]), pages.number(run["run_attempt"])
    selection = {"target": target, "auth_environment": environment,
                 "profile": config.profile, "sha": run["head_sha"]}
    if target == "preview":
        pr_number = current_preview_pr(gh, run)
        selection.update(branch=f"pr-{pr_number}", pr_number=pr_number)
    else:
        pages.require(run["head_branch"] == "master", "Auth canonical build must come from master")
        current_master = pages.sha(gh.get("/git/ref/heads/master")["object"]["sha"])
        if current_master != run["head_sha"]:
            if environment == "production":
                raise pages.Superseded("Auth build no longer matches master; build the current revision")
            # An unrelated merge must not strand the last relevant Dev build.
            # Still require current master ancestry and forward-only publication.
            pages.require(gh.get(f"/compare/{run['head_sha']}...{current_master}")["status"] == "ahead",
                          "Auth Dev source is no longer on master")
        previous_sha = pages.sha(gh.get(f"/git/ref/heads/{config.destination.production_branch}")["object"]["sha"])
        if previous_sha != run["head_sha"]:
            pages.require(gh.get(f"/compare/{previous_sha}...{run['head_sha']}")["status"] == "ahead",
                          "Non-forward auth deployment change")
        selection.update(branch=config.destination.production_branch, previous_sha=previous_sha)
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
    return {**selection, "run_id": run_id, "run_attempt": attempt,
            "artifact_id": pages.number(artifact["id"]), "artifact_digest": pages.digest(artifact["digest"])}


def prepare(gh, event, state, *, environment="production", target="production"):
    config = auth_target(environment)
    plan = select_plan(gh, event, target, environment=environment)
    pages.require(not state.exists() and not state.is_symlink(), "Auth deployment state already exists")
    state.mkdir(mode=0o700, parents=False)
    archive_digest = pages.download_build_artifact(gh, plan, state,
                                                  archive_name=AUTH_ARCHIVE_NAME,
                                                  expected_profile=config.profile)
    files = extract_static(state / "web.tar.gz", state / "assets", archive_digest)
    (state / "plan.json").write_text(json.dumps({"selection": plan, "archive_digest": archive_digest,
                                                "files": files}))
    return plan


def deploy(gh, event, state, *, environment="production", target="production"):
    def selected(gh, event, prepared_target):
        pages.require(prepared_target == target, "Prepared Auth target does not match publisher job")
        return select_plan(gh, event, target, environment=environment)

    pages.deploy(gh, event, state, destination=auth_target(environment).destination, selector=selected)


def require_publisher_environment(environment="production", target="production"):
    config = auth_target(environment)
    pages.require(target in {"production", "preview"}
                  and (environment == "development" or target == "production"),
                  "Invalid Auth destination kind")
    pages.require(os.environ.get(config.enabled_variable) == "true",
                  "Auth publication is disabled for the selected environment")
    events = {"workflow_dispatch"}
    if environment == "development":
        events = {"workflow_run"} if target == "preview" else {"workflow_run", "workflow_dispatch"}
    pages.require(os.environ.get("GITHUB_REF") == "refs/heads/master"
                  and os.environ.get("GITHUB_EVENT_NAME") in events,
                  "Auth publisher must execute a permitted event on protected master")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=["prepare", "deploy"])
    parser.add_argument("--state", type=Path, required=True)
    parser.add_argument("--environment", choices=tuple(TARGETS), default="production")
    parser.add_argument("--target", choices=("production", "preview"), default="production")
    args = parser.parse_args()
    require_publisher_environment(args.environment, args.target)
    checkout_sha = subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip()
    pages.require(checkout_sha == pages.sha(os.environ.get("GITHUB_SHA")),
                  "Auth publisher checkout must match the trusted workflow SHA")
    event = json.loads(Path(os.environ["GITHUB_EVENT_PATH"]).read_text())
    pages.require((os.environ["GITHUB_EVENT_NAME"] == "workflow_run") == ("workflow_run" in event),
                  "Auth publisher event mismatch")
    gh = pages.GitHub(pages.API("https://api.github.com", os.environ.get("GH_TOKEN", "")),
                      os.environ["GITHUB_REPOSITORY"], int(os.environ["GITHUB_REPOSITORY_ID"]))
    state = args.state.resolve()
    pages.require(state.parent == Path(os.environ["RUNNER_TEMP"]).resolve(),
                  "Auth state must be outside the checkout in runner temp")
    if args.command == "prepare":
        prepare(gh, event, state, environment=args.environment, target=args.target)
    else:
        deploy(gh, event, state, environment=args.environment, target=args.target)


if __name__ == "__main__":
    try:
        main()
    except (pages.Rejected, ValueError, KeyError, TypeError, OSError,
            HTTPError, URLError, subprocess.SubprocessError):
        print("Auth Pages publisher rejected the operation. Check activation, source, artifact, and destination prerequisites.",
              file=sys.stderr)
        sys.exit(1)
