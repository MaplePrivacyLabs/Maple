#!/usr/bin/env python3
"""Retain Agent changes until the selected master lane actually validates them."""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import sys

from agent_change_detection import classify_paths


ROOT = Path(__file__).resolve().parents[2]
REPOSITORY = "MaplePrivacyLabs/Maple"
WORKFLOW = ".github/workflows/agent-desktop-build.yml"
VERIFIERS = frozenset(
    f"Verify downloaded {platform} ({profile})"
    for platform in ("macOS", "Linux", "Windows") for profile in ("dev", "prod")
)
CI_WORKFLOW = ".github/workflows/agent-ci.yml"
CI_VERIFIERS = frozenset({
    "Agent (blacksmith-8vcpu-ubuntu-2404)",
    "Agent (macos-26)",
    "Agent (windows-latest)",
    "Agent provider (blacksmith-8vcpu-ubuntu-2404)",
    "Agent provider (macos-26)",
    "Agent provider (windows-latest)",
    "Agent Linux release binary",
})


def api(path: str) -> dict:
    result = subprocess.run(
        ["gh", "api", "--method", "GET", path],
        check=True, capture_output=True, text=True, timeout=15,
    )
    return json.loads(result.stdout)


def git(root: Path, *args: str) -> subprocess.CompletedProcess:
    return subprocess.run(
        ["git", *args], cwd=root, check=True, capture_output=True, timeout=15,
    )


def select_master(
    head: str, current_run_id: int, root: Path = ROOT, *, lane: str = "desktop",
) -> tuple[bool, str]:
    if not re.fullmatch(r"[0-9a-f]{40}", head):
        raise ValueError("invalid current commit")
    if lane not in ("desktop", "ci"):
        raise ValueError("unknown Agent validation lane")
    workflow = CI_WORKFLOW if lane == "ci" else WORKFLOW
    verifiers = CI_VERIFIERS if lane == "ci" else VERIFIERS
    runs = api(
        f"repos/{REPOSITORY}/actions/workflows/{workflow.rsplit('/', 1)[1]}/runs"
        "?branch=master&status=success&per_page=100"
    )["workflow_runs"]
    for run in runs:
        if (
            run["id"] == current_run_id
            or run["repository"]["full_name"] != REPOSITORY
            or run["head_repository"]["full_name"] != REPOSITORY
            or run["head_branch"] != "master"
            or run["event"] not in ("push", "workflow_dispatch")
            or run["path"] != workflow
            or run["status"] != "completed"
            or run["conclusion"] != "success"
        ):
            continue
        base = run["head_sha"]
        if not re.fullmatch(r"[0-9a-f]{40}", base):
            raise ValueError("invalid baseline commit")
        ancestor = subprocess.run(
            ["git", "merge-base", "--is-ancestor", base, head],
            cwd=root, capture_output=True, timeout=15,
        )
        if ancestor.returncode == 1:
            continue
        if ancestor.returncode != 0:
            raise ValueError("baseline history unavailable")
        jobs = api(
            f"repos/{REPOSITORY}/actions/runs/{run['id']}/jobs?filter=latest&per_page=100"
        )
        if jobs["total_count"] != len(jobs["jobs"]):
            raise ValueError("incomplete baseline job list")
        verified = {
            job["name"] for job in jobs["jobs"]
            if job["status"] == "completed" and job["conclusion"] == "success"
        }
        if not verifiers <= verified:
            # Selector-only successes and incomplete profiles cannot advance
            # the baseline past an Agent change that still needs packaging.
            continue
        paths = git(root, "diff", "--no-renames", "--name-only", "-z", base, head).stdout
        changed = [path.decode("utf-8", errors="surrogateescape")
                   for path in paths.split(b"\0") if path]
        selected = classify_paths(changed)
        return selected, (
            f"Agent inputs {'changed' if selected else 'unchanged'} since fully verified "
            f"master run {run['id']} at {base}; current commit {head}."
        )
    return True, "No fully verified ancestor in the last 100 successful master runs; building."


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--lane", choices=("desktop", "ci"), default="desktop")
    parser.add_argument("--head", default=os.environ.get("GITHUB_SHA", ""))
    parser.add_argument("--run-id", type=int, default=os.environ.get("GITHUB_RUN_ID", "0"))
    args = parser.parse_args()
    selected = True
    if (os.environ.get("GITHUB_EVENT_NAME") != "push"
            or os.environ.get("GITHUB_REPOSITORY") != REPOSITORY
            or os.environ.get("GITHUB_REF") != "refs/heads/master"):
        reason = "Manual or non-master event; building without a baseline lookup."
    else:
        try:
            selected, reason = select_master(args.head, args.run_id, lane=args.lane)
        except (KeyError, TypeError, ValueError, OSError, subprocess.SubprocessError):
            reason = "Master baseline lookup or diff unavailable; building."
    print(reason, file=sys.stderr)
    print(f"agent={'true' if selected else 'false'}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
