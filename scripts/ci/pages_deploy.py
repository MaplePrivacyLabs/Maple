#!/usr/bin/env python3
"""Publish verified static artifacts using trusted default-branch code only.

The build workflow is unprivileged. Its archive is data, never configuration or
executable code on this runner. See docs/pages-deployments.md for the threat model.
"""

from __future__ import annotations

import argparse
from dataclasses import dataclass
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import time
from urllib.error import HTTPError, URLError
from urllib.parse import urlsplit
from urllib.request import HTTPRedirectHandler, Request, build_opener

# -I discards PYTHONPATH and the working directory. This is the trusted checkout.
sys.path.insert(0, str(Path(__file__).resolve().parent))
from pages_artifact import extract_static, read_preview_zip  # noqa: E402

PROJECT = "maple"
SUBDOMAIN = "maple-ca8.pages.dev"
PRODUCTION_BRANCH = "pages-production"
PREVIEW_WORKFLOW = "pages-preview-build.yml"
RELEASE_WORKFLOW = "release.yml"
MAX_DOWNLOAD = 200 * 1024 * 1024


@dataclass(frozen=True)
class Destination:
    """Trusted source configuration, never read from a build artifact."""

    project: str
    subdomain: str
    production_branch: str
    environment: str
    public_url: str
    headers: str | None = None
    allow_direct_upload: bool = False


APP_DESTINATION = Destination(PROJECT, SUBDOMAIN, PRODUCTION_BRANCH,
                              "pages-production", "https://trymaple.ai")


class Rejected(ValueError):
    """Category-only message: never echo attacker-controlled input or API bodies."""


class Superseded(Rejected):
    """A once eligible deployment no longer owns its target."""


def require(condition, message):
    if not condition:
        raise Rejected(message)


def number(value):
    require(type(value) is int and value > 0, "Invalid numeric identifier")
    return value


def sha(value):
    require(isinstance(value, str) and re.fullmatch(r"[0-9a-f]{40}", value), "Invalid source SHA")
    return value


def digest(value):
    require(isinstance(value, str) and re.fullmatch(r"sha256:[0-9a-f]{64}", value), "Missing GitHub asset digest")
    return value.removeprefix("sha256:")


class NoRedirect(HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


class API:
    def __init__(self, base, token):
        require(bool(token), "Required API credential is not configured")
        self.base, self.token = base, token
        self.opener = build_opener(NoRedirect)

    def request(self, path, method="GET", data=None, accept="application/vnd.github+json"):
        require(path.startswith("/") and not path.startswith("//"), "Invalid API path")
        headers = {"Authorization": f"Bearer {self.token}", "Accept": accept,
                   "User-Agent": "Maple-Pages-publisher", "X-GitHub-Api-Version": "2022-11-28"}
        body = None if data is None else json.dumps(data).encode()
        if body is not None:
            headers["Content-Type"] = "application/json"
        return self.opener.open(Request(self.base + path, data=body, headers=headers, method=method), timeout=60)

    def json(self, path, method="GET", data=None):
        with self.request(path, method, data) as response:
            body = response.read(8 * 1024 * 1024 + 1)
        require(len(body) <= 8 * 1024 * 1024, "API response exceeds limit")
        return json.loads(body)

    def download(self, path, destination, limit=MAX_DOWNLOAD, expected_size=None, expected_digest=None,
                 accept="application/octet-stream"):
        # Only the constructed GitHub API endpoint receives Authorization.
        # GitHub asset redirects are signed URLs; never forward the token to them.
        try:
            response = self.request(path, accept=accept)
        except HTTPError as error:
            require(error.code in {301, 302, 303, 307, 308}, "Artifact download rejected")
            url = error.headers.get("Location", "")
            error.close()
            for _ in range(5):
                parsed = urlsplit(url)
                require(parsed.scheme == "https" and parsed.hostname and not parsed.username
                        and not parsed.password and parsed.port in {None, 443}, "Invalid artifact redirect")
                try:
                    response = self.opener.open(Request(url, headers={"User-Agent": "Maple-Pages-publisher"}), timeout=60)
                    break
                except HTTPError as redirected:
                    require(redirected.code in {301, 302, 303, 307, 308}, "Artifact redirect rejected")
                    url = redirected.headers.get("Location", "")
                    redirected.close()
            else:
                raise Rejected("Too many artifact redirects")
        size, hasher = 0, hashlib.sha256()
        with response, destination.open("xb") as output:
            while chunk := response.read(1024 * 1024):
                size += len(chunk)
                require(size <= limit, "Artifact exceeds download limit")
                hasher.update(chunk)
                output.write(chunk)
        require(expected_size is None or size == expected_size, "Artifact size mismatch")
        require(expected_digest is None or hasher.hexdigest() == expected_digest, "Artifact digest mismatch")


class GitHub:
    def __init__(self, api, repository, repository_id):
        require(re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repository), "Invalid repository")
        self.api, self.repository = api, repository
        self.repository_id = number(repository_id)
        self.root = "/repos/" + repository

    def get(self, path):
        return self.api.json(self.root + path)

    def write(self, path, data, method="POST"):
        return self.api.json(self.root + path, method, data)

    def run(self, run_id, workflow, event, attempt=None):
        run = self.get(f"/actions/runs/{number(run_id)}")
        expected_workflow = self.get(f"/actions/workflows/{workflow}")
        require(run["repository"]["id"] == self.repository_id
                and run["head_repository"]["id"] == self.repository_id, "Foreign repository run")
        require(run["workflow_id"] == expected_workflow["id"]
                and run["path"] == f".github/workflows/{workflow}", "Unexpected build workflow")
        require(run["event"] == event and run["status"] == "completed"
                and run["conclusion"] == "success", "Build has not succeeded")
        if attempt is not None and run["run_attempt"] != number(attempt):
            raise Superseded("Build attempt was superseded")
        number(run["run_attempt"])
        sha(run["head_sha"])
        return run


def preview_plan(gh, event):
    trigger = event["workflow_run"]
    require(trigger["event"] in {"pull_request", "push"}, "Unexpected preview event")
    run = gh.run(trigger["id"], PREVIEW_WORKFLOW, trigger["event"], trigger["run_attempt"])
    require(run["head_sha"] == trigger["head_sha"], "Trigger SHA mismatch")
    pr_number = None
    if run["event"] == "push":
        require(run["head_branch"] == "master", "Unexpected preview branch")
        if gh.get("/git/ref/heads/master")["object"]["sha"] != run["head_sha"]:
            raise Superseded("Master preview was superseded")
        branch = "master"
    else:
        # workflow_run.pull_requests can be empty. Ask GitHub for PRs associated
        # with this exact commit, then independently validate the current head.
        candidates = gh.get(f"/commits/{run['head_sha']}/pulls?per_page=100")
        matches = []
        for candidate in candidates:
            pr = gh.get(f"/pulls/{number(candidate['number'])}")
            if (pr["state"] == "open" and pr["base"]["ref"] == "master"
                    and pr["base"]["repo"]["id"] == gh.repository_id
                    and pr["head"].get("repo") and pr["head"]["repo"]["id"] == gh.repository_id
                    and pr["head"]["sha"] == run["head_sha"]
                    and pr["head"]["ref"] == run["head_branch"]):
                matches.append(pr)
        if not matches:
            raise Superseded("No current internal PR owns this preview")
        require(len(matches) == 1, "Ambiguous preview PR")
        pr_number = number(matches[0]["number"])
        branch = f"pr-{pr_number}"
    artifact_name = f"maple-pages-preview-{run['id']}-{run['run_attempt']}"
    artifacts = gh.get(f"/actions/runs/{run['id']}/artifacts?per_page=100")
    require(artifacts["total_count"] <= 100, "Too many build artifacts")
    matches = [a for a in artifacts["artifacts"] if a["name"] == artifact_name and not a["expired"]]
    require(len(matches) == 1, "Expected one preview artifact from this attempt")
    artifact = matches[0]
    require(0 < artifact["size_in_bytes"] <= MAX_DOWNLOAD, "Preview artifact size invalid")
    return {"target": "preview", "profile": "pr", "sha": run["head_sha"], "branch": branch,
            "pr_number": pr_number, "run_id": number(run["id"]), "run_attempt": run["run_attempt"],
            "artifact_id": number(artifact["id"]), "artifact_digest": digest(artifact["digest"])}


def release_asset(release, name):
    matches = [a for a in release["assets"] if a["name"] == name and a["state"] == "uploaded"]
    require(len(matches) == 1, "Expected one release asset")
    asset = matches[0]
    require(type(asset["size"]) is int and 0 < asset["size"] <= MAX_DOWNLOAD, "Release asset size invalid")
    return {"id": number(asset["id"]), "size": asset["size"], "digest": digest(asset["digest"])}


def production_plan(gh, event):
    release = gh.get("/releases/latest")
    require(not release["draft"] and not release["prerelease"], "Release is not stable")
    tag = release["tag_name"]
    require(re.fullmatch(r"v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", tag), "Invalid stable release tag")
    release_sha = sha(gh.get(f"/commits/{tag}")["sha"])
    if "workflow_run" in event:
        trigger = event["workflow_run"]
        if trigger["head_branch"] != tag:
            raise Superseded("Release is no longer the latest stable release")
        run = gh.run(trigger["id"], RELEASE_WORKFLOW, "release", trigger["run_attempt"])
    else:
        runs = gh.get(f"/actions/workflows/{RELEASE_WORKFLOW}/runs?event=release&head_sha={release_sha}&per_page=100")
        candidates = [r for r in runs["workflow_runs"] if r["head_branch"] == tag]
        require(bool(candidates), "No release build exists for this stable release")
        run = gh.run(candidates[0]["id"], RELEASE_WORKFLOW, "release")
    require(run["head_sha"] == release_sha and run["head_branch"] == tag, "Release build SHA mismatch")
    require(gh.get(f"/compare/{release_sha}...master")["status"] in {"ahead", "identical"}, "Release is not on master")
    current_sha = sha(gh.get(f"/git/ref/heads/{PRODUCTION_BRANCH}")["object"]["sha"])
    if current_sha != release_sha:
        require(gh.get(f"/compare/{current_sha}...{release_sha}")["status"] == "ahead", "Non-forward production change")
    return {"target": "production", "profile": "release", "sha": release_sha, "branch": PRODUCTION_BRANCH,
            "release_id": number(release["id"]), "release_tag": tag, "previous_sha": current_sha,
            "run_id": number(run["id"]), "run_attempt": run["run_attempt"],
            "archive": release_asset(release, "maple-web-dist.tar.gz"),
            "checksum": release_asset(release, "web-final.sha256")}


def select_plan(gh, event, target):
    repo = gh.get("")
    require(repo["id"] == gh.repository_id and repo["default_branch"] == "master", "Unexpected repository identity")
    require(target in {"preview", "production"}, "Invalid deployment target")
    return preview_plan(gh, event) if target == "preview" else production_plan(gh, event)


def prepare(gh, event, target, state):
    require(not state.exists(), "Deployment state already exists")
    state.mkdir(mode=0o700, parents=False)
    plan = select_plan(gh, event, target)
    archive = state / "web.tar.gz"
    if target == "preview":
        archive_digest = download_build_artifact(gh, plan, state)
    else:
        for key, path in (("archive", archive), ("checksum", state / "web.sha256")):
            asset = plan[key]
            gh.api.download(gh.root + f"/releases/assets/{asset['id']}", path,
                            limit=MAX_DOWNLOAD if key == "archive" else 4096,
                            expected_size=asset["size"], expected_digest=asset["digest"])
        archive_digest = plan["archive"]["digest"]
        checksum = (state / "web.sha256").read_text()
        require(re.fullmatch(re.escape(archive_digest) + r"  (?:(?:apps/maple-research/)?frontend/src-tauri/target/reproducibility/)?maple-web-dist\.tar\.gz\n?", checksum),
                "Release checksum manifest mismatch")
    files = extract_static(archive, state / "assets", archive_digest)
    (state / "plan.json").write_text(json.dumps({"selection": plan, "archive_digest": archive_digest, "files": files}))
    return plan


def download_build_artifact(gh, plan, state, **artifact_options):
    zipped = state / "artifact.zip"
    gh.api.download(gh.root + f"/actions/artifacts/{plan['artifact_id']}/zip", zipped,
                    expected_digest=plan["artifact_digest"], accept="application/vnd.github+json")
    manifest = read_preview_zip(zipped, plan["sha"], plan["run_id"], plan["run_attempt"],
                                state / "web.tar.gz", **artifact_options)
    return manifest["archive_sha256"]


def cloudflare_project(cf, account, target, destination=APP_DESTINATION):
    project = cf.json(f"/accounts/{account}/pages/projects/{destination.project}")
    require(project.get("success") is True, "Cloudflare project lookup failed")
    project = project["result"]
    require(project["name"] == destination.project and project["subdomain"] == destination.subdomain
            and project["production_branch"] == destination.production_branch, "Cloudflare project identity mismatch")
    if target == "production":
        source = project.get("source")
        no_git_source = destination.allow_direct_upload and source is None
        disabled_git_builds = (isinstance(source, dict) and isinstance(source.get("config"), dict)
                               and source["config"].get("production_deployments_enabled") is False)
        require(no_git_source or disabled_git_builds,
                "Disable Cloudflare automatic production builds before enabling the publisher")
    return project


def wrangler_environment(account, token, workdir):
    require(re.fullmatch(r"[0-9a-f]{32}", account), "Invalid Cloudflare account ID")
    require(bool(token), "Cloudflare deployment token is not configured")
    # No GH token, runner command files, NODE_OPTIONS, Bun/npm configuration,
    # proxies, inherited CF overrides, agent hints or parent home directory.
    environment = {"PATH": os.environ["PATH"], "HOME": str(workdir / "home"),
                   "TMPDIR": str(workdir / "tmp"), "CI": "true", "NO_COLOR": "1",
                   "CLOUDFLARE_ACCOUNT_ID": account, "CLOUDFLARE_API_TOKEN": token,
                   "WRANGLER_SEND_METRICS": "false", "WRANGLER_LOG_LEVEL": "error",
                   "WRANGLER_OUTPUT_FILE_PATH": str(workdir / "wrangler.jsonl")}
    for name in ("SSL_CERT_FILE", "NIX_SSL_CERT_FILE"):
        if name in os.environ:
            environment[name] = os.environ[name]
    (workdir / "home").mkdir()
    (workdir / "tmp").mkdir()
    return environment


def require_clean_wrangler_ancestors(workdir):
    # Wrangler searches above cwd for project/configuration files. The artifact
    # is a sibling, and even an unexpected runner-level config must fail closed.
    for directory in (workdir, *workdir.parents):
        for name in ("wrangler.toml", "wrangler.json", "wrangler.jsonc", ".wrangler",
                     "package.json", "functions", ".env", ".env.local", ".dev.vars"):
            require(not (directory / name).exists() and not (directory / name).is_symlink(),
                    "Unexpected deployment configuration above Wrangler working directory")


def run_wrangler(plan, assets, account, token, workdir, destination=APP_DESTINATION):
    root = Path(__file__).resolve().parents[2]
    executable = root / "services/updates/node_modules/wrangler/bin/wrangler.js"
    require(executable.is_file(), "Pinned Wrangler is not installed")
    node = shutil.which("node")
    require(node is not None, "Pinned Node runtime is not available")
    require_clean_wrangler_ancestors(workdir)
    environment = wrangler_environment(account, token, workdir)
    command = [node, str(executable), "pages", "deploy", str(assets), "--project-name", destination.project,
               "--branch", plan["branch"], "--commit-hash", plan["sha"], "--commit-dirty=false",
               "--commit-message", f"Maple {plan['profile']} {plan['sha']}", "--no-bundle"]
    # Never echo Wrangler output: remote errors and artifact names are untrusted.
    subprocess.run(command, cwd=workdir, env=environment, stdin=subprocess.DEVNULL,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=True, timeout=900)
    output = workdir / "wrangler.jsonl"
    require(output.is_file() and output.stat().st_size < 1024 * 1024, "Missing Wrangler result")
    results = [json.loads(line) for line in output.read_text().splitlines()]
    results = [result for result in results if result.get("type") == "pages-deploy-detailed"]
    require(len(results) == 1, "Unexpected Wrangler result")
    result = results[0]
    require(result["pages_project"] == destination.project and result["environment"] == plan["target"]
            and result["deployment_trigger"]["metadata"]["commit_hash"] == plan["sha"], "Wrangler deployment mismatch")
    require(re.fullmatch(r"[0-9a-f-]{36}", result["deployment_id"]), "Invalid deployment ID")
    require(re.fullmatch(r"https://[0-9a-f]{8}\." + re.escape(destination.subdomain), result["url"]), "Unexpected deployment URL")
    return result


def verify_deployment(cf, account, plan, result, destination=APP_DESTINATION):
    path = f"/accounts/{account}/pages/projects/{destination.project}"
    for _ in range(30):
        response = cf.json(path + f"/deployments/{result['deployment_id']}")
        require(response.get("success") is True, "Cloudflare deployment lookup failed")
        deployment = response["result"]
        require(deployment["environment"] == plan["target"] and deployment["project_name"] == destination.project
                and deployment["url"] == result["url"]
                and deployment["deployment_trigger"]["metadata"]["commit_hash"] == plan["sha"]
                and deployment["deployment_trigger"]["metadata"]["branch"] == plan["branch"], "Cloudflare deployment mismatch")
        if (deployment["latest_stage"]["name"] == "deploy"
                and deployment["latest_stage"]["status"] == "success"):
            if plan["target"] == "preview":
                return
            project = cloudflare_project(cf, account, "production", destination)
            if project["canonical_deployment"]["id"] == result["deployment_id"]:
                return
        require(deployment["latest_stage"]["status"] not in {"failure", "canceled"}, "Cloudflare deployment failed")
        time.sleep(2)
    raise Rejected("Cloudflare deployment did not become active")


def report(gh, plan, result, destination=APP_DESTINATION):
    environment = destination.environment if plan["target"] == "production" else f"pages-{plan['branch']}"
    deployment = gh.write("/deployments", {"ref": plan["sha"], "environment": environment,
                          "auto_merge": False, "required_contexts": [], "transient_environment": plan["target"] == "preview",
                          "production_environment": plan["target"] == "production",
                          "description": "Verified static Pages artifact"})
    public_url = destination.public_url if plan["target"] == "production" else result["url"]
    gh.write(f"/deployments/{number(deployment['id'])}/statuses", {"state": "success", "environment_url": public_url,
             "description": "Cloudflare deployment verified; application smoke is separate", "auto_inactive": True})
    if plan.get("pr_number"):
        # A small comment update uses only validated numbers, SHA and Cloudflare URL.
        marker = "<!-- maple-pages-preview -->"
        body = f"{marker}\nMaple development preview: {result['url']}\n\nCommit: `{plan['sha']}`\n\nUses development API, billing, flags and PCR configuration. Cloudflare Access applies."
        comments = gh.get(f"/issues/{plan['pr_number']}/comments?per_page=100")
        own = [c for c in comments if c["user"]["login"] == "github-actions[bot]" and c["body"].startswith(marker)]
        if own:
            gh.write(f"/issues/comments/{number(own[-1]['id'])}", {"body": body}, "PATCH")
        else:
            gh.write(f"/issues/{plan['pr_number']}/comments", {"body": body})


def add_trusted_headers(assets, destination):
    """Only trusted publisher source may supply Pages response-header rules."""
    if destination.headers is not None:
        with (assets / "_headers").open("x", encoding="utf-8", newline="\n") as output:
            output.write(destination.headers)


def deploy(gh, event, state, *, destination=APP_DESTINATION, selector=select_plan):
    saved = json.loads((state / "plan.json").read_text())
    plan = saved["selection"]
    if selector(gh, event, plan["target"]) != plan:
        raise Superseded("Deployment selection changed before upload")
    account, token = os.environ.get("CLOUDFLARE_ACCOUNT_ID", ""), os.environ.get("CLOUDFLARE_API_TOKEN", "")
    require(re.fullmatch(r"[0-9a-f]{32}", account), "Invalid Cloudflare account ID")
    cf = API("https://api.cloudflare.com/client/v4", token)
    cloudflare_project(cf, account, plan["target"], destination)
    # Re-extract under a new directory; no cache or previously extracted files
    # can alter what the credential-bearing Wrangler process sees.
    with tempfile.TemporaryDirectory(prefix="maple-pages-upload-", dir=state.parent) as temp:
        workspace = Path(temp)
        files = extract_static(state / "web.tar.gz", workspace / "assets", saved["archive_digest"])
        require(files == saved["files"], "Prepared artifact changed")
        add_trusted_headers(workspace / "assets", destination)
        workdir = workspace / "runner"
        workdir.mkdir()
        result = run_wrangler(plan, workspace / "assets", account, token, workdir, destination)
        verify_deployment(cf, account, plan, result, destination)
    if plan["target"] == "production":
        if selector(gh, event, "production") != plan:
            raise Superseded("Release changed during deployment; inspect the deployed result")
        if plan["previous_sha"] != plan["sha"]:
            updated = gh.write(f"/git/refs/heads/{destination.production_branch}", {"sha": plan["sha"], "force": False}, "PATCH")
            require(updated["object"]["sha"] == plan["sha"], "Production ref update failed")
    else:
        if selector(gh, event, "preview") != plan:
            raise Superseded("Preview changed during deployment; a newer preview is required")
    report(gh, plan, result, destination)
    summary = (f"### Maple Pages {plan['target']}\n\n"
               f"- Source: `{plan['sha']}`; profile: `{plan['profile']}`.\n"
               f"- Deployment: {result['url']}\n"
               f"- Cloudflare deployment ID: `{result['deployment_id']}`.\n"
               "- Verified Cloudflare deployment state and commit; production also verifies the active deployment.\n"
               "- Login, chat, browser configuration and Access behavior require separate application smoke checks.\n")
    if os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(os.environ["GITHUB_STEP_SUMMARY"], "a") as output:
            output.write(summary)
    print(f"Verified Pages {plan['target']} deployment for {plan['sha']}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=["prepare", "deploy"])
    parser.add_argument("--target", choices=["preview", "production"])
    parser.add_argument("--state", type=Path, required=True)
    args = parser.parse_args()
    require(os.environ.get("GITHUB_REF") == "refs/heads/master", "Publisher must run from protected master")
    require(os.environ.get("GITHUB_EVENT_NAME") in {"workflow_run", "workflow_dispatch"}, "Unexpected publisher event")
    checkout_sha = subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip()
    require(checkout_sha == sha(os.environ.get("GITHUB_SHA")), "Publisher checkout must be the trusted workflow SHA")
    event = json.loads(Path(os.environ["GITHUB_EVENT_PATH"]).read_text())
    require((os.environ["GITHUB_EVENT_NAME"] == "workflow_run") == ("workflow_run" in event), "Publisher event mismatch")
    gh = GitHub(API("https://api.github.com", os.environ.get("GH_TOKEN", "")),
                os.environ["GITHUB_REPOSITORY"], int(os.environ["GITHUB_REPOSITORY_ID"]))
    state = args.state.resolve()
    require(state.parent == Path(os.environ["RUNNER_TEMP"]).resolve(), "State must be outside the checkout in runner temp")
    if args.command == "prepare":
        require(args.target is not None, "Deployment target is required")
        prepare(gh, event, args.target, state)
    else:
        deploy(gh, event, state)


if __name__ == "__main__":
    try:
        main()
    except Superseded as error:
        print(f"Pages deployment skipped: {error}")
        # Stop later steps as well; rerun from the newest source instead.
        sys.exit(1)
    except (Rejected, ValueError, KeyError, TypeError, OSError, HTTPError, URLError, subprocess.SubprocessError):
        print("Pages publisher rejected the operation. Check workflow provenance, artifact validation, and documented configuration prerequisites.", file=sys.stderr)
        sys.exit(1)
