#!/usr/bin/env python3
"""Select and run the ci.yml lanes defined in scripts/ci/lanes.py.

  github       ci.yml's plan job: write the `lanes` and matrix outputs and a summary
  run LANE     run one lane's command in its flake, as CI does (`just lane LANE`)
  only NAME    another workflow's push selector: print selected=true|false
  explain      show what changed paths select (stdin, or --base/--head)
  check-rust   the Agent flake's rustc must equal the Windows rustup pin
  lint         the table classifies every tracked file and matches ci.yml
"""

from __future__ import annotations

import argparse
import functools
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shlex
import shutil
import subprocess
import sys
from typing import Dict, Iterable, List, Optional, Sequence

from lanes import (AGENT_WINDOWS_RUST, CI_INFRA, DISPATCH, INERT, LANES, MATRIX_JOBS, NIGHTLY, NOT_YET_TRACKED,
                   PR, PR_EVENTS, PUSH, QUEUE, SELECTORS, Lane)

ROOT = Path(__file__).resolve().parents[2]
CI_WORKFLOW = ROOT / ".github/workflows/ci.yml"
WINDOWS_SETUP = ROOT / ".github/actions/setup-agent-windows/action.yml"
Selection = Dict[str, List[str]]  # lane id -> why it was selected


@functools.lru_cache(maxsize=None)
def _pattern(glob: str) -> "re.Pattern[str]":
    regex = re.escape(glob).replace(r"\*\*/", "(?:[^/]+/)*").replace(r"\*\*", ".*").replace(r"\*", "[^/]*")
    return re.compile(regex)


def matches(path: str, globs: Iterable[str]) -> bool:
    globs = tuple(globs)
    return (any(_pattern(g).fullmatch(path) for g in globs if not g.startswith("!"))
            and not any(_pattern(g[1:]).fullmatch(path) for g in globs if g.startswith("!")))


def lane_globs(lane: Lane, event: Optional[str] = None) -> tuple:
    if event in PR_EVENTS and lane.pr_paths is not None:
        return lane.pr_paths
    return lane.paths + lane.also + (lane.pr_paths or ())


def malformed(path: str) -> bool:
    return not path or path.startswith("/") or ".." in path.split("/")


def inert(path: str) -> bool:
    return not malformed(path) and matches(path, INERT)


def classified(path: str) -> bool:
    """Owned by some lane, active or legacy."""
    return not malformed(path) and (
        matches(path, CI_INFRA) or any(matches(path, lane_globs(lane)) for lane in LANES))


def active(event: str) -> List[Lane]:
    return [lane for lane in LANES if lane.legacy is None and event in lane.events]


def everything(event: str, why: str) -> Selection:
    return {lane.id: [why] for lane in active(event)}


def select(event: str, paths: Iterable[str], labels: Sequence[str] = ()) -> Selection:
    """Active lanes that the changed paths and PR labels select for `event`."""
    lanes = active(event)
    chosen: Selection = {}
    for path in paths:
        if inert(path):
            continue
        if not classified(path):
            # Fail safe: an unknown path may be an input of anything.
            for lane in lanes:
                chosen.setdefault(lane.id, []).append(f"unclassified {path}")
            continue
        infra = matches(path, CI_INFRA)
        for lane in lanes:
            if (infra and lane.gate) or matches(path, lane_globs(lane, event)):
                chosen.setdefault(lane.id, []).append(path)
    for label in labels:
        for lane in lanes:
            if label in lane.labels:
                chosen.setdefault(lane.id, []).append(f"label {label}")
    return chosen


def git(*args: str) -> str:
    return subprocess.run(["git", *args], cwd=ROOT, check=True, capture_output=True, text=True).stdout


def changed_paths(base: str, head: str, merge_base: bool) -> Optional[List[str]]:
    """Paths changed from `base` (or its merge base with `head`) to `head`; None
    when there is no usable base, such as a new branch or a force-push."""
    for sha in (base, head):
        if not re.fullmatch(r"[0-9a-f]{40}", sha or "") or set(sha) == {"0"}:
            return None
        if subprocess.run(["git", "cat-file", "-e", f"{sha}^{{commit}}"], cwd=ROOT,
                          capture_output=True).returncode:
            return None
    if merge_base:
        base = git("merge-base", base, head).strip()
    return [p for p in git("diff", "--name-only", "--no-renames", "-z", base, head).split("\0") if p]


def pr_labels(repository: str, number: int) -> List[str]:
    # Read live, so adding a label and re-running the job takes effect.
    if not (os.environ.get("GH_TOKEN") or os.environ.get("GITHUB_TOKEN")):
        return []
    result = subprocess.run(["gh", "api", "--paginate", f"repos/{repository}/issues/{number}/labels",
                             "--jq", ".[].name"], check=True, capture_output=True, text=True)
    return result.stdout.splitlines()


def plan_event(event: str, payload: dict) -> Selection:
    if event == DISPATCH:
        names = [n.strip() for n in (os.environ.get("LANES_INPUT") or "all").split(",") if n.strip()]
        if names in ([], ["all"]):
            return everything(event, "dispatch: all")
        unknown = sorted(set(names) - {lane.id for lane in active(event)})
        if unknown:
            raise ValueError(f"not an active lane: {', '.join(unknown)}")
        return {name: ["dispatch"] for name in names}
    if event == NIGHTLY:
        return everything(event, "nightly")
    labels: List[str] = []
    if event == PR:
        pull = payload["pull_request"]
        base, head = pull["base"]["sha"], pull["head"]["sha"]
        labels = pr_labels(os.environ["GITHUB_REPOSITORY"], pull["number"])
    elif event == QUEUE:
        base, head = payload["merge_group"]["base_sha"], payload["merge_group"]["head_sha"]
    elif event == PUSH:
        base, head = payload["before"], payload["after"]
    else:
        raise ValueError(f"unsupported event: {event}")
    paths = changed_paths(base, head, merge_base=event == PR)
    if paths is None:
        return everything(event, f"no usable diff for {base[:12]}..{head[:12]}")
    return select(event, paths, labels)


def cache_key(lane: Lane) -> str:
    digest = hashlib.sha256()
    for name in lane.cache_files:
        digest.update((ROOT / name).read_bytes())
    return digest.hexdigest()[:16]


def outputs(selected: Selection) -> Dict[str, str]:
    """`lanes` maps every active lane to whether it runs; each matrix job gets its entries."""
    result = {"lanes": {lane.id: lane.id in selected for lane in LANES if lane.legacy is None}}
    for job in MATRIX_JOBS:
        result[job] = [{
            "id": lane.id,
            "runner": os.environ.get(lane.runner.variable or "") or lane.runner.label,
            "timeout": lane.timeout,
            "flake": lane.flake or "",
            "cache": lane.cache,
            "cache_key": cache_key(lane) if lane.cache else "",
            "workspaces": lane.workspaces,
        } for lane in LANES if lane.id in selected and lane.job == job]
    return {name: json.dumps(value, separators=(",", ":")) for name, value in result.items()}


def summary(event: str, selected: Selection) -> str:
    lines = [f"### Lanes for this {event}", ""]
    if not selected:
        return "\n".join(lines + ["No lane selected."]) + "\n"
    lines += ["| Lane | Why |", "|---|---|"]
    for lane in LANES:
        reasons = selected.get(lane.id)
        if reasons:
            more = f" (+{len(reasons) - 1} more)" if len(reasons) > 1 else ""
            lines.append(f"| `{lane.id}` | `{reasons[0]}`{more} |")
    return "\n".join(lines) + "\n"


def github() -> int:
    event = os.environ["GITHUB_EVENT_NAME"]
    try:
        selected = plan_event(event, json.loads(Path(os.environ["GITHUB_EVENT_PATH"]).read_text()))
    except Exception as error:  # noqa: BLE001 - every failure is handled the same way
        if event in PR_EVENTS:
            print(f"::error::lane selection failed: {error!r}")
            return 1
        # Master, nightly and dispatch runs verify everything rather than nothing.
        print(f"::warning::lane selection failed ({error!r}); selecting every lane")
        selected = everything(event, "selection failed")
    with open(os.environ["GITHUB_OUTPUT"], "a") as output:
        output.writelines(f"{name}={value}\n" for name, value in outputs(selected).items())
    text = summary(event, selected)
    if os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(os.environ["GITHUB_STEP_SUMMARY"], "a") as output:
            output.write(text)
    print(text, end="")
    return 0


def explain(event: str, base: Optional[str], head: Optional[str]) -> int:
    if base and head:
        paths = changed_paths(git("rev-parse", base).strip(), git("rev-parse", head).strip(),
                              merge_base=event == PR)
        if paths is None:
            print(f"no usable diff for {base}..{head}")
            return 1
    else:
        data = sys.stdin.read()
        paths = [p for p in re.split("\0" if "\0" in data else "\n", data) if p.strip()]
    selected = select(event, paths)
    print(f"{event}: {len(paths)} changed paths")
    for lane in LANES:
        reasons = selected.get(lane.id)
        if reasons:
            more = f" (+{len(reasons) - 1} more)" if len(reasons) > 1 else ""
            print(f"  {lane.id}: {reasons[0]}{more}")
    unknown = [p for p in paths if not inert(p) and not classified(p)]
    if unknown:
        print("  unclassified, so every lane runs: " + ", ".join(unknown))
    legacy = sorted({f"{lane.id} ({lane.legacy})" for path in paths for lane in LANES
                     if lane.legacy and not inert(path) and matches(path, lane_globs(lane))})
    if legacy:
        print("  still run by their own workflows: " + "; ".join(legacy))
    if not selected:
        print("  no ci.yml lane")
    return 0


def run(name: str) -> int:
    lane = next((lane for lane in LANES if lane.id == name), None)
    if lane is None or not lane.run:
        print(f"{name}: no ci.yml command in scripts/ci/lanes.py", file=sys.stderr)
        return 2
    command = ["bash", "-euo", "pipefail", "-c", f"cd {shlex.quote(lane.workdir)} && {lane.run}"]
    if platform.system() == "Windows":
        # No Nix on Windows: setup-agent-windows pins the toolchain instead. Process
        # creation searches System32 (WSL's bash.exe) before PATH, so take Git Bash from PATH.
        command[0] = shutil.which("bash") or "bash"
    elif lane.flake:
        command = ["nix", "develop", "--no-update-lock-file", lane.flake, "-c", *command]
    print("+ " + " ".join(shlex.quote(part) for part in command), flush=True)
    return subprocess.run(command, cwd=ROOT, env={**os.environ, **dict(lane.env)}).returncode


def only(name: str) -> int:
    """agent-desktop-build.yml's changes job: did this push change `name`'s inputs?

    Anything but a push with a usable diff (dispatch, a new branch, a force-push,
    an error) selects the build, as do unclassified paths."""
    paths = None
    try:
        if os.environ.get("GITHUB_EVENT_NAME") == PUSH:
            payload = json.loads(Path(os.environ["GITHUB_EVENT_PATH"]).read_text())
            paths = changed_paths(payload["before"], payload["after"], merge_base=False)
    except Exception as error:  # noqa: BLE001 - fail safe: build
        print(f"::warning::{name} selection failed ({error!r}); building", file=sys.stderr)
    selected = paths is None or any(
        not inert(path) and (not classified(path) or matches(path, SELECTORS[name])) for path in paths)
    print(f"{name}: {'no usable diff' if paths is None else f'{len(paths)} changed paths'}, "
          f"selected={str(selected).lower()}", file=sys.stderr)
    print(f"selected={str(selected).lower()}")
    return 0


def check_rust(expected: str = AGENT_WINDOWS_RUST) -> int:
    reported = subprocess.run(["rustc", "--version"], check=True, capture_output=True, text=True).stdout
    version = reported.split()[1]
    if version != expected:
        print(f"::error::The Agent flake has rustc {version}, but setup-agent-windows pins {expected}. Update "
              "AGENT_WINDOWS_RUST in scripts/ci/lanes.py and .github/actions/setup-agent-windows together.")
        return 1
    print(f"rustc {version} matches the Windows rustup pin")
    return 0


def tracked_files() -> List[str]:
    if (ROOT / ".git").exists():  # tracked, plus new files not yet added
        return [p for p in git("ls-files", "-z", "--cached", "--others", "--exclude-standard").split("\0") if p]
    # A flake check sees a copy of exactly the tracked files, without .git.
    return sorted(str(p.relative_to(ROOT)) for p in ROOT.rglob("*") if p.is_file())


def lint_problems(files: Sequence[str]) -> List[str]:
    problems = []
    globs = ({g.lstrip("!") for g in INERT + CI_INFRA + sum(SELECTORS.values(), ())}
             | {g.lstrip("!") for lane in LANES for g in lane_globs(lane)})
    problems += [f"matches no tracked file: {g}" for g in sorted(globs - NOT_YET_TRACKED)
                 if not any(_pattern(g).fullmatch(f) for f in files)]
    problems += [f"unclassified: {f}" for f in files if not inert(f) and not classified(f)]
    ids = [lane.id for lane in LANES]
    problems += [f"duplicate lane: {i}" for i in sorted({i for i in ids if ids.count(i) > 1})]
    workflow = CI_WORKFLOW.read_text()
    jobs = set(re.findall(r"^  ([A-Za-z0-9_-]+):", workflow.split("\njobs:\n", 1)[1], re.M))
    expected = {"plan", "ci-ok", *MATRIX_JOBS} | {lane.job for lane in LANES if lane.legacy is None}
    if jobs != expected:
        problems.append(f"ci.yml jobs {sorted(jobs)} != plan, ci-ok, matrix and special lane jobs {sorted(expected)}")
    if f"toolchain: {AGENT_WINDOWS_RUST}\n" not in WINDOWS_SETUP.read_text():
        problems.append(f"setup-agent-windows does not pin Rust {AGENT_WINDOWS_RUST}")
    declared = set(re.findall(r"^\s+- (\S+)$", (ROOT / ".github/actionlint.yaml").read_text(), re.M))
    for lane in LANES:
        if lane.runner.label not in declared and not re.match(r"(ubuntu|macos|windows)-", lane.runner.label):
            problems.append(f"{lane.id}: runner {lane.runner.label} is not in .github/actionlint.yaml")
    return problems


def main(argv: Optional[Sequence[str]] = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    modes = parser.add_subparsers(dest="mode", required=True)
    modes.add_parser("github")
    modes.add_parser("run").add_argument("lane")
    modes.add_parser("only").add_argument("name", choices=sorted(SELECTORS))
    explain_parser = modes.add_parser("explain")
    explain_parser.add_argument("--event", default=PR, choices=(PR, QUEUE, PUSH))
    explain_parser.add_argument("--base")
    explain_parser.add_argument("--head")
    modes.add_parser("check-rust")
    modes.add_parser("lint")
    args = parser.parse_args(argv)
    if args.mode == "github":
        return github()
    if args.mode == "run":
        return run(args.lane)
    if args.mode == "only":
        return only(args.name)
    if args.mode == "explain":
        return explain(args.event, args.base, args.head)
    if args.mode == "check-rust":
        return check_rust()
    files = tracked_files()
    problems = lint_problems(files)
    print("\n".join(problems) if problems else f"lanes.py classifies all {len(files)} tracked files")
    return 1 if problems else 0


if __name__ == "__main__":
    raise SystemExit(main())
