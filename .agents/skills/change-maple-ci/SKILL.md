---
name: change-maple-ci
description: Change, trigger, or debug Maple's GitHub Actions CI. Covers the ci.yml lane planner (scripts/ci/plan.py, scripts/ci/lanes.py), the one required check ci-ok, ci:* labels (ci:full, ci:package, ci:windows, ci:ios, ci:android, ci:research-dev-macos, ci:eif), re-runs, ci.yml dispatch, nightly runs, runners and override variables, caches, timeouts, the pre-commit hook, guard and contract tests, and red or slow runs. Use it to add or change a lane or workflow, to get Windows, packaging, iOS, Android or EIF evidence on a PR, to decide what runs on a PR and what on master, or to explain why a check ran, did not run, or failed. Use release-maple for releases, signing and publication.
---

# Change Maple CI

Load this skill before you edit `.github/`, `scripts/ci/`, `.githooks/` or
the root flake checks, or when you need a CI result a PR does not get by
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
   pull requests only if its warm runs finish in about 8 minutes. Slower work
   runs by label, nightly, or as master's real builds. Why: the team ships
   many small PRs, and slow lanes crept back in one path selector at a time.
2. **Keep master deployable.** Merge only with `ci-ok` green. When master goes
   red, fix forward or revert first. Why: master pushes start signed builds
   and feed the next release.
3. **Add checks only as lanes in `scripts/ci/lanes.py`, each with a
   `scripts/ci/test_plan.py` case.** Never add `paths:` to `ci.yml`, a new
   workflow that runs on pull requests, another required check, or a bespoke
   guard script. Why: one table drives CI, the pre-commit hook and
   `just lane`, and `ci-ok` must report on every PR.
4. **Never put `[skip ci]`, `[ci skip]` or similar in a commit message or PR
   title.** Why: master plans each push from the previous commit, so a
   skipped commit is never checked.
5. **Pull requests never save an Actions cache, and master runs are never
   cancelled.** Why: a PR can rewrite its own YAML, so its caches are
   untrusted, and a cancelled master commit is never verified.
6. **A lane saves caches only into its own runner vendor's store. `ci.yml`
   never writes a key that a release restores.** Why: Blacksmith and GitHub
   cannot read each other's caches, and releases restore their keys by prefix.
7. **A failing `test_contracts.py` or `test_workflow_policy.py` means your
   change is wrong.** Never edit `release.yml`, a publisher, a signing
   environment, PCR approvals or OpenSecret approval gating to make a check
   pass. Why: releases, update feeds and EIF approvals trust those seams.
8. **Never rename `agent-desktop-build.yml`, `ios-dev-testflight.yml`, or a
   workflow, job or artifact that a publisher or the release preflight looks
   up.** Why: run numbers are build numbers, and consumers find producers by
   name.
9. **Never push to `master`, and never dispatch or re-run a signing or
   publishing workflow, to try a CI change.** Why: a master push can sign
   builds and upload to TestFlight. Re-running or dispatching `ci.yml` is fine.
10. **Keep fat LTO for Agent release builds, and do not propose thin LTO.**
    Why: the owner chose smaller downloads over a faster master build.
11. **Treat OpenSecret CI as reproducible TEE infrastructure.** Keep every EIF
    builder that writes FlakeHub on one runner class. Keep `os-eif` to
    same-repository PRs to `master`. Never give OIDC or FlakeHub to a new job.
    Why: PCR reproducibility depends on identical, trusted builders.
12. **Test behaviour, or a contract another file consumes. Never test a
    workflow's or script's text, and never hand-copy lists of names.** Why:
    text tests break on harmless edits and catch nothing.
13. **Do not add process.** Add no approval steps, required checks or safety
    switches unless the owner asks. Why: a team of four wants fast
    iteration, and master's push run plus fixing forward is the backstop.

The reasons in full are in [design](references/design.md).

## How CI works

Every `ci.yml` run has three stages:

1. **`plan`** runs `plan.py github`. It diffs the change, matches each path
   against `lanes.py`, and adds lanes for the PR's labels. Its job summary
   lists each selected lane with its reason.
2. **Lanes** run in the generic `nix` and `windows` matrix jobs. A lane that
   needs services, OIDC, artifacts or another workflow's cache gets a special
   job named after it. Every lane runs `plan.py run <id>`.
3. **`ci-ok`** runs `plan.py verdict`. It passes when every selected lane
   succeeded and nothing unselected failed. It is the only required check.

Selection rules:

- A PR is judged on its own diff from the merge base. Each layer of a stack
  gets its own `ci-ok`.
- Root and component-level docs, READMEs and hooks, and `.agents/**`, are
  inert: they select no lane but what the OpenSecret selector routes. Deeper
  files can be build inputs, so a README inside a crate selects its lanes.
- A path that no lane owns selects every lane, label-only builds included. So
  does a diff the planner cannot compute (a new branch or a force-push).
- Editing `ci.yml`, `.github/actions/**`, `plan.py` or `lanes.py` selects
  every gate lane. It never selects the slow `gate=False` lanes.
- A selection error fails a PR's `plan`; master, nightly and dispatch runs
  select every lane instead.

| Trigger | What runs |
| --- | --- |
| Pull request (any base) | Lanes selected by paths and labels. A new push cancels the superseded run. `opensecret-eif.yml`, `pages-preview-build.yml`, `auth-pages-ci.yml` and `proxy-rust.yml` may also run on matching paths. They are advisory, not required. |
| Merge | Auto-merge is on. There is no merge queue and no up-to-date rule (`ci.yml` already handles `merge_group` if a queue is ever enabled). The master push run catches semantic conflicts. |
| Master push | `ci.yml` runs the push lanes, adding the full Agent Linux and macOS suites for Agent changes. The Research and Auth PR lanes do not run here; their master workflows cover them. The run is never cancelled and saves caches. Also: signed builds (Agent: Prod only), release-evidence workflows and CodeQL. |
| Nightly (UTC) | 04:17 `ci.yml` nightly lanes: full Windows suite, packaging rehearsals, `deps-*` advisories, cache refresh. 05:28 CodeQL. 05:47 `nightly.yml`: Bun audits, the SQLCipher provider suite on three OSes, and the single `ci-nightly` issue. 10:37 Agent Dev desktop builds, which the issue does not cover. |
| Dispatch | `ci.yml` with lane ids ([below](#dispatch-ciyml)). |

## Trigger what you need

### Labels

Most PRs need no label. A label adds lanes to that PR's plan. The `labels=`
fields in `lanes.py` are authoritative.

| Label | Adds | Use when |
| --- | --- | --- |
| `ci:windows` | Agent Windows Clippy (already on every Agent PR), the native updater tests, the full Windows suite | Path, filesystem, process or terminal code behaves differently on Windows |
| `ci:package` | Unsigned Agent packages and Research desktop bundles on all three OSes, plus the Research Dev macOS app | Packaging or release scripts, flakes, toolchains, or bundled assets, entitlements or updater metadata change |
| `ci:ios`, `ci:android` | The Research iOS simulator or Android build | Research changes reach native mobile code or mobile-only plugins that their paths do not select |
| `ci:research-dev-macos` | The unsigned Research Dev app | Research Dev OAuth profile work |
| `ci:eif` | Dev and prod OpenSecret EIF candidates (`os-eif`) | The selector misses a measured input. Works only on a same-repository PR to `master` that edits no PCR approval |
| `ci:full` | Every PR lane except `os-eif` | Toolchain, lockfile-wide or risky cross-cutting changes before merge |

- Adding a label starts nothing. Push, or use **Re-run all jobs**: `plan`
  reads labels when it runs.
- **Re-run failed jobs** reuses the earlier `plan`, with its old lanes,
  labels and quarantine.
- Labelled builds can take 15 to 90 minutes, and every push re-runs them while
  the label stays. Remove it once you have the evidence.
- Labels act only on `pull_request` runs. iOS, Android and EIF builds are
  also path-selected by their native or measured inputs.

### Dispatch `ci.yml`

```sh
gh workflow run ci.yml --ref <branch> -f lanes=agent-full-windows,deps-sdk
```

- `lanes` takes comma-separated ids. `all`, the default, runs every lane
  active for dispatch.
- `os-eif` cannot be dispatched. Any id that is not a dispatch lane, a typo
  included, does not fail: the plan falls back to every dispatch lane,
  packaging builds included. Check ids in `lanes.py` first.
- Dispatch runs never save caches, so they warm nothing. Never use one to
  satisfy a PR's `ci-ok`; use labels.

### Master-only builds

Signed builds never run on PRs; never push to `master` to get one.
`agent-desktop-build.yml` builds Agent Prod packages on pushes that change
Agent inputs (`plan.py only agent-desktop`) and Dev packages nightly; a
dispatch signs both. Research signed builds follow
`scripts/ci/change_detection.py`. These belong to the release process
([boundary](#production-boundary)).

## Change CI

[Changing CI](references/changing-ci.md) has the field reference, special
jobs, runners, caches and guard tests. In short:

1. Edit one `Lane(...)` in `lanes.py`. `paths` are its own inputs (the hook
   reads only these), `also` is CI-only fan-out to consumers, and
   `pr_paths=()` makes it label-only on PRs.
2. Add a `test_plan.py` case: which paths and labels select it on a PR and on
   a push, and whether it runs nightly.
3. Prefer the `nix` and `windows` matrices. Keep the work in a script or
   `just` recipe so `just lane <id>` runs it anywhere.
4. Own or mark inert every new path. The OpenSecret selector runs every
   OpenSecret lane for a new top-level directory too.
5. Check a lane's warm duration before path-selecting it on PRs. Over about
   8 minutes, use `pr_paths=()` with a label, nightly, or master.

- **Runners.** Blacksmith for compiles people wait on, free GitHub runners
  for light or unwatched work, GitHub-hosted for attestation, signing,
  verification and Windows install smokes ([CI runners](../../../docs/ci.md#runners)).
  The `CI_*_RUNNER` repository variables move lanes in an outage, no PR needed.
- **Caches.** Lane caches use `Swatinem/rust-cache` with prefix `ci-v2`. Only
  master pushes and the nightly run save them. Never let an action save
  implicitly on a PR path.
- **Timeouts.** Size them for a cold run on the home runner, never for warm
  times. The 10-minute rule decides which lanes PRs wait on; it is not a
  timeout.
- **Never touch to go green:** release and publisher workflows,
  `ci_evidence.py`, signing environments, `pcr*.json`,
  `opensecret_change_detection.py`, or the allowlists in the policy and
  contract tests (adding an entry is an owner decision).

## Validate before pushing

Run from the repository root:

```sh
python3 scripts/ci/plan.py lint                      # every file classified; ci.yml matches the table
python3 scripts/ci/plan.py explain --base origin/master --head HEAD
python3 scripts/ci/plan.py explain --event push --base origin/master --head HEAD
just lane <id>    # each selected lane this OS can run
just lane repo    # every root flake check, for .github, scripts/ci, flake or hook edits
```

For a faster loop on `scripts/ci` or workflow edits:

```sh
nix develop --no-update-lock-file .#ci -c python3 -I -m unittest discover -s scripts/ci -p 'test_*.py'
SYS=$(nix eval --impure --raw --expr builtins.currentSystem)
nix build --no-link --no-update-lock-file .#checks.$SYS.ci-scripts .#checks.$SYS.workflows .#checks.$SYS.release-gates
```

- Docs and skills are inert, so a docs-only PR runs no docs check. After
  editing them, run `just lane repo` (or `ci-scripts`) yourself.
- `explain` also reads paths from stdin. It ignores labels, quarantine and
  the `os-eif` candidate rules, and has no schedule or dispatch event.
- `lint` also reads untracked, unignored files: a stray scratch file is an
  unclassified path. The tests need yq-go, which the `.#ci` shell provides.
- macOS and Windows lanes need those OSes. `os-sdk-e2e` needs local
  PostgreSQL. `os-eif` needs Linux ARM64 and `EIF_MODE=dev` or `prod`.

## Debug a red or slow run

Read `plan`'s summary first: it says which lanes ran and why. `ci-ok`'s error
names the failing job; for `nix` or `windows`, open the row named after the
lane. Then follow [debugging](references/debugging.md). The common cases:

| Symptom | Response |
| --- | --- |
| Nix setup fails with HTTP 416, `SSL_read` or "no substituter" from `cache.nixos.org` | Transient network fault. Re-run the job |
| "Lost communication with the server", or a runner that vanished | Lost runner. Re-run failed jobs |
| `plan` failed on a PR | Usually a label read failure: **Re-run all jobs**. A `CI_QUARANTINE` typo fails every run until the variable is fixed |
| Many more lanes than expected | Unclassified path or unusable diff. Classify the path in `lanes.py` |
| The lane you edited did not run | CI edits select gate lanes only. Add its label, then **Re-run all jobs** |
| A lane slower than usual | Cold cache after a lockfile, toolchain, prefix or runner-image change. The next master save warms it; do not cut the timeout |
| Heavy lanes queued for many minutes | Runner pool burst. Set `CI_LINUX_HEAVY_RUNNER=blacksmith-16vcpu-ubuntu-2404` (or another `CI_*_RUNNER`), **Re-run all jobs**, and unset it afterwards |
| `repo` fails a policy or contract test | Your change broke a rule or seam. Read the failing assertion and fix the change |
| Agent native Windows suite fails in a store test | Known intermittent flake. Re-run once; a repeat goes to the `ci-nightly` issue |

## Production boundary

CI decides whether a change is safe to merge. It never releases, signs,
publishes or deploys. That work belongs to `$release-maple` and the guides
below, and each step needs the owner's explicit go.

- A workflow, `scripts/ci` or Nix change on `master` makes the next Research
  release wait for fresh evidence. Tell the release owner.
- CodeQL is outside `ci-ok`; the release preflight needs its newest finished
  scan to be green.
- An `agent-desktop-build.yml` dispatch, a signed build or any publisher is a
  production action. Ask before running one.

| For | Read |
| --- | --- |
| Research releases, TestFlight, App Store and Play handoffs | [`$release-maple`](../release-maple/SKILL.md) |
| Contracts CI keeps with releases | [Repository workflows](../../../docs/repository-workflows.md#contracts) |
| Signing environments | [App signing environments](../../../docs/app-signing-environments.md) |
| Pages and Auth publication | [Pages deployments](../../../docs/pages-deployments.md) |
| Agent packages and updates | [Agent desktop builds](../../../apps/maple-agent/docs/desktop-builds.md), [Agent update publication](../../../services/agent-updates/README.md#publication-and-authority) |
| SDK publication | [SDK publishing](../../../docs/sdk-publishing.md#run-a-release) |
| OpenSecret EIFs and PCR approvals | [EIF CI and release](../../../services/opensecret/docs/nitro-deploy.md#ci-approval-checks), [PCR compatibility](../../../services/opensecret/docs/pcr-compatibility.md) |

## References

- [Design](references/design.md): why CI has this shape.
- [Changing CI](references/changing-ci.md): lane fields, special jobs, runners, caches, the hook, guard tests.
- [Debugging](references/debugging.md): re-run semantics, failure signatures, cold caches, fail-over.
