#!/usr/bin/env python3
"""Keep one open `ci-nightly` issue while a nightly check fails.

nightly.yml's report job runs this with its jobs' results in NEEDS. Failures
there, or in today's scheduled ci.yml run (also when it is missing or still
running), open the issue or comment on it; a green night closes it.
DRY_RUN=true prints instead of writing.
"""

from __future__ import annotations

from datetime import datetime, timezone
import json
import os
import subprocess
from typing import List, Optional, Tuple

LABEL = "ci-nightly"
TITLE = "Nightly CI failures"


def gh(path: str, method: str = "GET", payload: Optional[dict] = None) -> object:
    result = subprocess.run(["gh", "api", "--method", method, path, *(["--input", "-"] if payload else [])],
                            input=json.dumps(payload) if payload else None,
                            check=True, capture_output=True, text=True)
    return json.loads(result.stdout or "null")


def failures(needs: dict, ci_run: Optional[dict], today: str) -> List[str]:
    lines = [f"- `{job}`: {state.get('result')}" for job, state in sorted(needs.items())
             if state.get("result") != "success"]
    if ci_run is None or ci_run["created_at"][:10] != today:
        lines.append(f"- ci.yml: no scheduled run on master on {today}")
    elif ci_run.get("status") != "completed":
        lines.append(f"- ci.yml scheduled run of {today}: still {ci_run.get('status')} ({ci_run['html_url']})")
    elif ci_run.get("conclusion") != "success":
        lines.append(f"- ci.yml scheduled run of {today}: {ci_run.get('conclusion')} ({ci_run['html_url']})")
    return lines


def decide(lines: List[str], issue: Optional[int], run_url: str) -> Optional[Tuple[str, str]]:
    """The one write to make: create, comment or close (None when green and closed)."""
    if lines:
        day = datetime.now(timezone.utc).strftime("%Y-%m-%d")
        return ("comment" if issue else "create"), f"Nightly checks failed on {day} ({run_url}):\n\n" + "\n".join(lines)
    return ("close", f"Every nightly check passed ({run_url}).") if issue else None


def main() -> int:
    repo = os.environ["GITHUB_REPOSITORY"]
    server = os.environ.get("GITHUB_SERVER_URL", "https://github.com")
    run_url = f"{server}/{repo}/actions/runs/{os.environ['GITHUB_RUN_ID']}"
    runs = gh(f"repos/{repo}/actions/workflows/ci.yml/runs?event=schedule&branch=master&per_page=1")
    today = datetime.now(timezone.utc).strftime("%Y-%m-%d")
    lines = failures(json.loads(os.environ["NEEDS"]), (runs["workflow_runs"] or [None])[0], today)
    open_issues = [item["number"] for item in gh(f"repos/{repo}/issues?labels={LABEL}&state=open&per_page=100")
                   if "pull_request" not in item]
    issue = min(open_issues) if open_issues else None
    action = decide(lines, issue, run_url)
    if action is None:
        print("Every nightly check passed; no open ci-nightly issue.")
        return 0
    kind, body = action
    print(f"{kind} {f'#{issue}' if issue else 'a new issue'}:\n{body}")
    if os.environ.get("DRY_RUN") == "true":
        return 0
    if kind == "create":
        gh(f"repos/{repo}/issues", "POST", {"title": TITLE, "body": body, "labels": [LABEL]})
    else:
        gh(f"repos/{repo}/issues/{issue}/comments", "POST", {"body": body})
        if kind == "close":
            gh(f"repos/{repo}/issues/{issue}", "PATCH", {"state": "closed", "state_reason": "completed"})
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
