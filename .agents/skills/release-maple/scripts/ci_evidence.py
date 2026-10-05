#!/usr/bin/env python3
"""Read-only Research release CI evidence, including unchanged ancestor inputs."""

import argparse
import json
import re
import subprocess
import sys


WORKFLOWS = {
    "frontend-tests.yml": ("test-frontend",),
    "rust-tests.yml": ("rust-tests",),
    "web-build.yml": ("build-web-master", "verify-web-master-artifact"),
    "desktop-build.yml": (
        "build-macos", "build-linux", "build-windows",
        "verify-macos-desktop-artifacts", "verify-linux-desktop-artifacts",
        "verify-windows-desktop-artifacts", "verify-desktop-artifacts",
    ),
    "android-build.yml": ("build-android", "verify-android-artifacts"),
    "mobile-build.yml": ("build-ios", "verify-ios-artifacts", "submit-ios-testflight"),
    # CodeQL is unfiltered and must execute on the exact release commit.
    "codeql.yml": (),
}


class EvidenceError(RuntimeError):
    pass


def command(*args):
    return subprocess.check_output(args, text=True)


def api(repo, path):
    return json.loads(command("gh", "api", f"repos/{repo}/{path}"))


def unrelated_path(path):
    # Deliberately conservative: unknown files, shared CI/Nix, SDK, proxy and
    # every Research input invalidate reuse. Agent is a separate application.
    return path.startswith(("apps/maple-agent/", "docs/", ".agents/")) or path in {
        "AGENTS.md", "scripts/ci/test-release-gates.sh",
    }


def jobs_executed(jobs, required):
    by_name = {job["name"]: job for job in jobs}
    if not required:
        required = tuple(name for name in by_name if name.startswith("Analyze ("))
        if not required:
            raise EvidenceError("CodeQL analysis jobs are missing")
    if any(name not in by_name for name in required):
        raise EvidenceError("required jobs are missing")
    selected = [by_name[name] for name in required]
    if all(job.get("conclusion") == "skipped" for job in selected):
        return False
    if any(job.get("status") != "completed" or job.get("conclusion") != "success"
           for job in selected):
        raise EvidenceError("required jobs did not all execute successfully")
    return True


def eligible_runs(runs, repo, workflow):
    expected_path = f".github/workflows/{workflow}"
    return sorted((run for run in runs
                   if run.get("event") == "push" and run.get("head_branch") == "master"
                   and run.get("head_repository", {}).get("full_name") == repo
                   and run.get("path") == expected_path),
                  key=lambda run: (run["created_at"], run["id"]), reverse=True)


def select_evidence(runs, head, required, get_jobs, compatible, exact_only=False):
    for run in runs:
        sha = run["head_sha"]
        if not re.fullmatch(r"[0-9a-f]{40}", sha):
            raise EvidenceError("invalid run commit")
        if exact_only and sha != head:
            continue
        # Never choose an older green run over a newer failure or pending run.
        if run.get("status") != "completed" or run.get("conclusion") != "success":
            raise EvidenceError(f"run {run['id']} is {run.get('status')}/{run.get('conclusion')}")
        if not compatible(sha):
            raise EvidenceError(f"run {run['id']} is not an ancestor with unchanged Research inputs")
        if not jobs_executed(get_jobs(run["id"]), required):
            continue
        return {"run_id": run["id"], "url": run["html_url"], "head_sha": sha,
                "reused": sha != head}
    raise EvidenceError("no successful executed master push found in the bounded run history")


def trees_compatible(before, after):
    def entries(tree):
        if tree.get("truncated") is not False:
            raise EvidenceError("Git tree inventory is incomplete")
        return {entry["path"]: (entry["sha"], entry["mode"], entry["type"])
                for entry in tree["tree"] if entry["type"] != "tree"}
    left, right = entries(before), entries(after)
    return all(unrelated_path(path) for path in left.keys() | right.keys()
               if left.get(path) != right.get(path))


def collect(repo, head):
    compatibility = {head: True}
    trees = {}

    def tree(sha):
        if sha not in trees:
            trees[sha] = api(repo, f"git/trees/{sha}?recursive=1")
        return trees[sha]

    def compatible(sha):
        if sha not in compatibility:
            comparison = api(repo, f"compare/{sha}...{head}")
            if comparison["status"] not in ("ahead", "identical"):
                compatibility[sha] = False
            else:
                # Never use GitHub compare's truncated 300-file diff. Prefer
                # local objects, otherwise demand complete recursive Git trees.
                if subprocess.run(["git", "cat-file", "-e", f"{sha}^{{commit}}"],
                                  stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL).returncode == 0:
                    paths = command("git", "diff", "--name-only", "--no-renames", "-z", sha, head)
                    compatibility[sha] = all(unrelated_path(path) for path in paths.split("\0") if path)
                else:
                    compatibility[sha] = trees_compatible(tree(sha), tree(head))
        return compatibility[sha]

    evidence = {}
    for workflow, required in WORKFLOWS.items():
        # Filter locally: require the canonical workflow, master push and repo.
        runs = api(repo, f"actions/workflows/{workflow}/runs?per_page=100")["workflow_runs"]

        def get_jobs(run_id):
            data = api(repo, f"actions/runs/{run_id}/jobs?per_page=100")
            if data["total_count"] > len(data["jobs"]):
                raise EvidenceError("job inventory is incomplete")
            return data["jobs"]

        try:
            evidence[workflow] = select_evidence(
                eligible_runs(runs, repo, workflow), head, required, get_jobs,
                compatible, exact_only=workflow == "codeql.yml")
        except EvidenceError as error:
            raise EvidenceError(f"{workflow}: {error}") from error
    return evidence


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", required=True)
    parser.add_argument("--head", required=True)
    args = parser.parse_args()
    if args.repo != "MaplePrivacyLabs/Maple" or not re.fullmatch(r"[0-9a-f]{40}", args.head):
        parser.error("expected canonical Maple repository and full commit SHA")
    try:
        print(json.dumps(collect(args.repo, args.head), indent=2))
    except (EvidenceError, subprocess.CalledProcessError, KeyError, ValueError) as error:
        print(f"release preflight: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
