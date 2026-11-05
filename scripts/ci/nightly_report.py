#!/usr/bin/env python3
"""Keep one open `ci-nightly` issue while a nightly check fails.

nightly.yml's report job runs this with its jobs' results in NEEDS. Failures
there, or in today's scheduled ci.yml run (also when it is missing or still
running), open the issue or comment on it; a green night closes it. Lanes
quarantined in CI_QUARANTINE are listed too, so a quarantine stays visible.
GitHub starts scheduled runs hours after their cron time, so ci.yml's run may
not have started or finished yet: the report waits up to CI_WAIT_MINUTES
(default 0) for it. DRY_RUN=true prints instead of writing.
"""

from __future__ import annotations

from datetime import datetime, timezone
import json
import os
import subprocess
import time
from typing import List, Optional, Tuple

LABEL = "ci-nightly"
TITLE = "Nightly CI failures"
QUARANTINED = "is quarantined (`CI_QUARANTINE`)"
POLL_SECONDS = 60
# A hung API call fails instead of holding the job until its timeout.
GH_TIMEOUT_SECONDS = 60


def gh(path: str, method: str = "GET", payload: Optional[dict] = None) -> object:
    result = subprocess.run(["gh", "api", "--method", method, path, *(["--input", "-"] if payload else [])],
                            input=json.dumps(payload) if payload else None,
                            check=True, capture_output=True, text=True, timeout=GH_TIMEOUT_SECONDS)
    return json.loads(result.stdout or "null")


def scheduled_ci_run(repo: str, today: str, wait_seconds: float) -> Optional[dict]:
    """The newest scheduled ci.yml run on master, once today's has finished or after
    `wait_seconds`, whichever comes first. A failed or timed-out API call while waiting
    is retried."""
    deadline = time.monotonic() + wait_seconds
    while True:
        remaining = deadline - time.monotonic()
        try:
            runs = gh(f"repos/{repo}/actions/workflows/ci.yml/runs?event=schedule&branch=master&per_page=1")
        except (subprocess.CalledProcessError, subprocess.TimeoutExpired):
            if remaining <= 0:
                raise
        else:
            run = (runs["workflow_runs"] or [None])[0]
            if remaining <= 0 or (run is not None and run["created_at"][:10] == today
                                  and run.get("status") == "completed"):
                return run
        time.sleep(min(POLL_SECONDS, remaining))


def failures(needs: dict, ci_run: Optional[dict], today: str, quarantine: str = "") -> List[str]:
    lines = [f"- `{job}`: {state.get('result')}" for job, state in sorted(needs.items())
             if state.get("result") != "success"]
    lines += [f"- `{name}` {QUARANTINED}: its failures do not fail `ci-ok`"
              for name in sorted({name.strip() for name in quarantine.split(",") if name.strip()})]
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
        if any(QUARANTINED not in line for line in lines):
            heading = f"Nightly checks failed on {day} ({run_url}):"
        else:  # a green night with an active quarantine keeps the issue open
            heading = f"Nightly checks passed on {day} ({run_url}); active quarantines:"
        return ("comment" if issue else "create"), heading + "\n\n" + "\n".join(lines)
    return ("close", f"Every nightly check passed ({run_url}).") if issue else None


def main() -> int:
    repo = os.environ["GITHUB_REPOSITORY"]
    server = os.environ.get("GITHUB_SERVER_URL", "https://github.com")
    run_url = f"{server}/{repo}/actions/runs/{os.environ['GITHUB_RUN_ID']}"
    today = datetime.now(timezone.utc).strftime("%Y-%m-%d")
    ci_run = scheduled_ci_run(repo, today, float(os.environ.get("CI_WAIT_MINUTES") or 0) * 60)
    lines = failures(json.loads(os.environ["NEEDS"]), ci_run, today, os.environ.get("CI_QUARANTINE", ""))
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
