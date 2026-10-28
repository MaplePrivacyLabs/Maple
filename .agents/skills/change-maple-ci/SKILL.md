---
name: change-maple-ci
description: Change, trigger or debug Maple's GitHub Actions CI - the ci.yml lane planner (scripts/ci/plan.py, lanes.py), the required ci-ok check, ci:* labels, re-runs, runners, caches, the hook and guard tests. Use it to add or change a lane or workflow, to get platform, packaging or EIF evidence on a PR, or to explain why a check ran, did not run, failed or was slow. Releases and signing belong to release-maple.
---

# Change Maple CI

Load this skill before you edit `.github/`, `scripts/ci/`, `.githooks/` or
the root flake checks, or when a PR needs a CI result it does not get by
default. [CI](../../../docs/ci.md) is the human reference;
[repository workflows](../../../docs/repository-workflows.md) owns caches,
contracts and the signed builds. Code beats any list here:
`scripts/ci/lanes.py` is the source of truth for lanes, paths, labels,
events, runners and timeouts. Ask the planner instead of guessing:

```sh
python3 scripts/ci/plan.py explain --base origin/master --head HEAD
```

## Hard rules

1. **Keep every PR's `ci-ok` well under 10 minutes.** Path-select a lane on
   PRs only if its warm runs take about 8 minutes or less; slower work runs by
   label, nightly, or as master's real builds. `research-ios`,
   `research-android` and `os-eif` are owner-accepted exceptions, the only PR
   proof for their inputs. Why: slow lanes crept back one selector at a time.
2. **Keep master deployable.** Merge only with `ci-ok` green; when master goes
   red, fix forward or revert first. Why: master pushes start signed builds
   and feed the next release.
3. **Add checks only as lanes in `lanes.py`, each with a `test_plan.py`
   case.** Never add `paths:` to `ci.yml`, a new PR workflow, another required
   check, or a bespoke guard script. Why: one table drives CI, the hook and
   `just lane`, and `ci-ok` must report on every PR.
4. **Never put `[skip ci]` or similar in a commit message or PR title.** Why:
   master plans each push from the previous commit, so a skipped commit is
   never checked.
5. **Lanes never save a cache on a PR, and master runs are never
   cancelled.** Why: a PR can rewrite its own YAML, and a cancelled master
   commit is never verified. The EIF jobs' branch-scoped exceptions are in
   [design](references/design.md#pull-requests-never-save-caches).
6. **A lane saves only into its own runner vendor's cache store, and `ci.yml`
   never writes a key a release restores.** Why: Blacksmith and GitHub cannot
   read each other's caches, and releases restore their keys by prefix.
7. **A failing `test_contracts.py` or `test_workflow_policy.py` means your
   change is wrong.** Never edit release or publisher workflows,
   `ci_evidence.py`, signing environments, `pcr*.json`,
   `opensecret_change_detection.py` or the tests' allowlists to pass. Why:
   releases, update feeds and EIF approvals trust those seams.
8. **Never rename `agent-desktop-build.yml`, `ios-dev-testflight.yml`, or a
   workflow, job or artifact a publisher or the release preflight looks up.**
   Why: run numbers are build numbers; consumers find producers by name.
9. **Never push to `master`, or dispatch or re-run a signing or publishing
   workflow, to try a CI change.** Why: a master push can sign builds and
   upload to TestFlight. Re-running `ci.yml` is fine.
10. **Keep fat LTO for Agent release builds; do not propose thin LTO.** Why:
    the owner chose smaller downloads over a faster master build.
11. **Treat OpenSecret CI as reproducible TEE infrastructure.** Keep every
    FlakeHub EIF builder on one runner class, `os-eif` on same-repository PRs
    to `master`, and OIDC and FlakeHub away from new jobs. Why: PCR
    reproducibility depends on identical, trusted builders.
12. **Test behaviour, or a contract another file consumes.** Assert parsed
    structure and evaluated conditions; never grep prose or comments, and
    never hand-copy lists of names. Why: text tests break on harmless edits.
13. **Do not add process:** no approval steps, required checks or safety
    switches unless the owner asks. Why: a small team wants fast iteration,
    and master's push run plus fixing forward is the backstop.

The reasons in full are in [design](references/design.md).

## How CI works

Every `ci.yml` run has three stages:

1. **`plan`** (`plan.py github`) diffs the change, matches each path against
   `lanes.py`, adds lanes for the PR's labels, and lists each with its reason
   in the job summary.
2. **Lanes** run in the generic `nix` and `windows` matrices through
   `plan.py run <id>`. A lane that needs services, OIDC, artifacts or another
   workflow's cache gets a special job named after it.
3. **`ci-ok`** (`plan.py verdict`), the only required check, passes when every
   selected lane succeeded and nothing unselected failed.

Selection rules:

- A PR is judged on its own diff from the merge base. Each layer of a stack
  gets its own `ci-ok`.
- Root and component-level docs, READMEs and hooks, and `.agents/**`, are
  inert: they select no lane but what the OpenSecret selector routes. Deeper
  files can be build inputs, so a README inside a crate selects its lanes.
- A path that no lane owns selects every lane, label-only builds included. So
  does a diff the planner cannot compute (a new branch or a force-push).
- Editing `ci.yml`, `.github/actions/**`, `plan.py` or `lanes.py` selects
  every gate lane, whatever its `pr_paths`. It never selects `gate=False`
  lanes.
- A selection error fails a PR's `plan`; master, nightly and dispatch runs
  select every lane instead.

Typical PR selections (confirm with `explain`):

| Change | Lanes |
| --- | --- |
| Agent source | `agent-lint`, `agent-test` (Linux), `agent-macos`, `agent-windows` (Clippy). Warm `ci-ok` in about 5 minutes; `agent-windows` is the long pole |
| Agent `Cargo.lock` or `Cargo.toml` | Those four, plus `agent-windows-updater`, `agent-supply-chain`, `agent-provider` |
| Agent packaging or installer scripts | Only the four; packaging needs `ci:package` |
| Research frontend; Research `src-tauri` | `research-web`; `research-rust` |
| TypeScript SDK | `sdk-ts`, `os-sdk-e2e` |
| Rust SDK source | The four Agent lanes, `sdk-rust`, `proxy`, `research-rust`, `os-sdk-e2e` |
| OpenSecret source | `os-rust`, `os-sdk-e2e`, `os-flake-check`, and `os-eif` on a candidate PR |
| `ci.yml`, `plan.py`, `lanes.py`, `.github/actions/**` | Every gate lane |
| Root or component docs, `.agents/**` | Nothing |

| Trigger | What runs |
| --- | --- |
| Pull request | Lanes selected by paths and labels. A new push cancels the superseded run. A few advisory workflows also run on matching paths ([CI](../../../docs/ci.md)); they are not part of `ci-ok`. |
| Merge | Auto-merge is on. There is no merge queue and no up-to-date rule (`ci.yml` already handles `merge_group` if a queue is ever enabled). The master push run catches semantic conflicts. |
| Master push | `ci.yml` runs the push lanes, adding `agent-full-linux` and `agent-full-macos` for Agent changes. The Research and Auth PR lanes do not run here; their master workflows cover them. The run is never cancelled and saves caches. Also: signed builds (Agent: Prod only), release-evidence workflows and CodeQL. |
| Nightly (UTC) | 04:17 `ci.yml` nightly lanes: `agent-full-*`, packaging rehearsals, `deps-*` advisories, cache refresh. 05:28 CodeQL. 05:47 `nightly.yml`: Bun audits, the SQLCipher provider suite on three OSes, and the single `ci-nightly` issue. 10:37 Agent Dev desktop builds, which the issue does not cover. |
| Dispatch | `ci.yml` with lane ids ([below](#dispatch-ciyml)). |

## Trigger what you need

### Labels

Most PRs need no label. A label adds lanes to that PR's plan. The `labels=`
fields in `lanes.py` are authoritative.

| Label | Adds | Use when |
| --- | --- | --- |
| `ci:windows` | The native updater tests and the full Windows suite (Windows Clippy already runs on every Agent PR) | Path, filesystem, process or terminal code behaves differently on Windows |
| `ci:package` | Unsigned Agent packages and Research desktop bundles on all three OSes, plus the Research Dev macOS app | Packaging or release scripts, flakes, toolchains, or bundled assets, entitlements or updater metadata change |
| `ci:ios`, `ci:android` | The Research iOS simulator or Android build | Research changes reach native mobile code or mobile-only plugins that their paths do not select |
| `ci:research-dev-macos` | The unsigned Research Dev app | Research Dev OAuth profile work |
| `ci:eif` | Dev and prod OpenSecret EIF candidates (`os-eif`) | The selector misses a measured input. Works only on a same-repository PR to `master` that edits no PCR approval |
| `ci:full` | Every PR lane except `os-eif`, the full Agent suites included | Toolchain, lockfile-wide or risky cross-cutting changes before merge |

- Adding a label starts nothing: `plan` reads labels when it runs. Push, or
  use **Re-run all jobs**, which GitHub offers only on a finished run: cancel
  a running PR run first. With auto-merge on, a run planned before the label
  can still pass `ci-ok` and merge the PR without the labelled lanes.
- **Re-run failed jobs** reuses the earlier `plan`: old lanes, labels and
  quarantine.
- Labelled builds take well over 10 minutes, and every push re-runs them
  while the label stays. Remove it once you have the evidence.

### Dispatch `ci.yml`

```sh
gh workflow run ci.yml --ref <branch> -f lanes=agent-full-windows,deps-sdk
```

- `lanes` takes comma-separated ids; `all` (the default) runs every dispatch
  lane. So does any id that is not a dispatch lane, a typo or `os-eif`
  included: the plan falls back instead of failing. Check ids in `lanes.py`.
- Dispatch runs never save caches. Their `ci-ok` lands on the branch's head
  commit and may count as an open PR's required check, so never dispatch on
  a branch with an open PR; use labels.

### Master-only builds

Signed builds never run on PRs; never push to `master` to get one.
`agent-desktop-build.yml` builds Agent Prod packages on pushes that change
Agent inputs (`plan.py only agent-desktop`) and Dev packages nightly. For Dev
packages, download the newest nightly run's `maple-agent-dev-*` artifacts
(kept 30 days). With the owner's go,
`gh workflow run agent-desktop-build.yml --ref master` builds and signs Dev
and Prod now, and its Prod packages are publishable. Research signed builds
follow `scripts/ci/change_detection.py`. These belong to the release process
([boundary](#production-boundary)).

## Change CI

[Changing CI](references/changing-ci.md) has the field reference, special
jobs, runners, caches and guard tests. In short:

1. Edit one `Lane(...)` in `lanes.py`. `paths` are its own inputs (the hook
   reads only these), `also` is CI-only fan-out to consumers.
2. Add a `test_plan.py` case: which paths and labels select it on a PR and on
   a push, and whether it runs nightly.
3. Prefer the `nix` and `windows` matrices. Keep the work in a script or
   `just` recipe so `just lane <id>` runs it anywhere.
4. Own or mark inert every new path. The OpenSecret selector runs every
   OpenSecret lane for a new top-level directory too.
5. Check a lane's warm duration before path-selecting it on PRs. Over about
   8 minutes, set both `pr_paths=()` and `gate=False`: with `pr_paths=()`
   alone, a CI edit still selects it on PRs. It then runs by label (default
   `ci:full`), on master pushes and nightly; `events=PR_AND_NIGHTLY` also
   drops master pushes.

- **Runners.** Blacksmith for compiles people wait on; free GitHub runners
  for light work; GitHub-hosted for the Agent's signing and attestation,
  `windows-signing`, artifact verifiers, and the Windows updater tests and
  install smokes ([CI runners](../../../docs/ci.md#runners)).
- **Caches.** rust-cache, prefix `ci-v2`; only master pushes and the nightly
  run save. Never let an action save implicitly on a PR path.
- **Timeouts.** Size them for a cold run on the home runner. The 10-minute
  rule decides PR exposure; it is not a timeout.
- **Comments and docs move with behaviour.** When you change what a lane,
  label, cache or workflow does, fix every comment and doc line that describes
  it in the same PR: search for the lane, label and file names. A docs-only
  follow-up cannot fix comments in workflows and scripts without running their
  lanes, so stale ones linger.

## Validate before pushing

Run from the repository root:

```sh
python3 scripts/ci/plan.py lint                      # every file classified; ci.yml matches the table
python3 scripts/ci/plan.py explain --base origin/master --head HEAD
python3 scripts/ci/plan.py explain --event push --base "$(git merge-base origin/master HEAD)" --head HEAD
just lane <id>    # each selected lane this OS can run
just lane repo    # every root flake check, for .github, scripts/ci, flake, hook or doc edits
```

- Docs and skills are inert, so a docs-only PR runs no docs check. After
  editing them, run `just lane repo` yourself.
- `explain` also reads paths from stdin. It ignores labels and quarantine,
  has no schedule or dispatch event, and lists `os-eif` for any EIF input
  without the same-repository and base-`master` checks.
- `lint` also reads untracked, unignored files: a stray scratch file is an
  unclassified path. Lanes that need macOS, Windows, PostgreSQL or ARM64 are
  listed in [changing CI](references/changing-ci.md#run-lanes-locally).

## Debug a red or slow run

Read `plan`'s summary first, then `ci-ok`'s error, which names the failing
job (for `nix` or `windows`, open the row named after the lane).
[Debugging](references/debugging.md) has the signatures and fail-over steps.

| Symptom | Response |
| --- | --- |
| `plan` failed on a PR | Usually a label read failure: **Re-run all jobs**. A `CI_QUARANTINE` typo fails every run until the variable is fixed |
| Many more lanes than expected | Unclassified path or unusable diff. Classify the path in `lanes.py` |
| The lane you edited did not run | CI edits select gate lanes only. Add its label, then **Re-run all jobs** |
| A lane slower than usual, or queued | Cold cache or a runner pool burst; see [slow runs](references/debugging.md#slow-runs). Do not cut the timeout |
| `repo` fails a policy or contract test | Your change broke a rule or seam. Read the failing assertion and fix the change |
| Any other unexplained failure | Re-run the failed job once (infrastructure faults are common). A repeat is real: fix it |

## Production boundary

CI decides whether a change is safe to merge; it never releases, signs,
publishes or deploys. That work belongs to the release owner and the guides
below, and each step needs the owner's explicit go.

- A master change to any path outside `ci_evidence.unrelated_path` (anything
  but `apps/maple-agent/`, `docs/`, `.agents/` and root `AGENTS.md`) makes
  older Research evidence unusable. A release gets fresh evidence from its
  version-bump push, so never merge such a change between a bump and its
  release; tell the release owner.
- Files below a component's root, READMEs included, are build inputs: on
  master they can start signed Agent or Research builds.
- CodeQL is outside `ci-ok`; the release preflight needs its newest finished
  scan to be green.
- An `agent-desktop-build.yml` dispatch or re-run, a signed build or any
  publisher is a production action. Ask the release owner before running one.

Read [`$release-maple`](../release-maple/SKILL.md),
[contracts](../../../docs/repository-workflows.md#contracts),
[signing environments](../../../docs/app-signing-environments.md),
[Pages](../../../docs/pages-deployments.md),
[Agent builds](../../../apps/maple-agent/docs/desktop-builds.md),
[Agent updates](../../../services/agent-updates/README.md#publication-and-authority),
[SDK publishing](../../../docs/sdk-publishing.md#run-a-release),
[EIF CI](../../../services/opensecret/docs/nitro-deploy.md#ci-approval-checks)
and [PCR compatibility](../../../services/opensecret/docs/pcr-compatibility.md).

## References

- [Design](references/design.md): why CI has this shape.
- [Changing CI](references/changing-ci.md): lane fields, special jobs, runners, caches, the hook, guard tests.
- [Debugging](references/debugging.md): re-run semantics, failure signatures, cold caches, fail-over.
