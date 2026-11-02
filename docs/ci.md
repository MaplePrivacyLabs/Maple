# CI

`.github/workflows/ci.yml` runs every required pull-request check. Its one
verdict, `ci-ok`, is the only required status check: it passes when every lane
the planner selected succeeded, and an unselected lane may only be skipped.
`auth-pages-ci.yml` and `proxy-rust.yml` also run on matching pull requests,
and `opensecret-eif.yml` and `pages-preview-build.yml` on matching pull
requests to `master`; they are advisory and not part of `ci-ok`. Signed
builds, publication, caches and the contracts CI keeps are in [repository
workflows](repository-workflows.md). Agents changing or triggering CI load
[`$change-maple-ci`](../.agents/skills/change-maple-ci/SKILL.md).

## How lanes are chosen

A lane is one check, defined once in `scripts/ci/lanes.py`: its own paths, the
consumers it fans out to (`also`), events, labels, runner, Nix shell, command
and timeout. `scripts/ci/plan.py` (the `plan` job) selects lanes from the diff:

- A pull request is judged on its own diff from the merge base with its base
  branch, so each layer of a stack gets its own `ci-ok`. A master push uses
  `before..after`. (`ci.yml` also handles a merge-queue group, by its base and
  head, but the repository has no merge queue.) Without a usable diff, such
  as a new branch or a force-push, every lane runs.
- Docs, guides and component hooks select no lane but those the OpenSecret
  selector routes them to (below). A path that no lane owns selects every
  lane, the label-only builds included. Editing `ci.yml`, the planner or
  `.github/actions/` selects every gate lane, but not the slow builds that
  run only by label.
- If selection fails on a pull request, `plan` and `ci-ok` fail; master,
  nightly and dispatch runs select every lane instead.
- OpenSecret lanes are selected by `scripts/ci/opensecret_change_detection.py`,
  imported unchanged and asked about every changed path, docs included.
- Dependency audits (`deps-*`, cargo-deny) run only when their manifest,
  lockfile or `deny.toml` changes, with `ci:full`, and nightly. CI edits never
  select them, so a newly published advisory in those graphs reaches a PR only
  through its own manifests; the nightly run reports it. Two gate lanes still
  audit advisories whenever they are selected, CI edits included:
  `agent-supply-chain` (cargo-deny for the Agent targets) and `sdk-ts`
  (`bun audit` in the SDK publish build).

The `plan` job summary lists each selected lane and why. To preview locally:

```sh
python3 scripts/ci/plan.py explain --base origin/master --head HEAD
python3 scripts/ci/plan.py explain --event push --base "$(git merge-base origin/master HEAD)" --head HEAD
```

The second command shows what master would run after the merge; a push diff
does not use the merge base, so pass it yourself. `explain` also reads paths
from stdin. It applies no labels, and it lists `os-eif` for any EIF input
without the same-repository and base-`master` checks (it does drop it for a
PCR approval edit).

## Tiers

| When | What runs |
| --- | --- |
| Pull request | Selected lanes, each a few minutes; the iOS and Android builds when their native inputs change, and the OpenSecret EIF candidate for measured inputs. Desktop bundles and packaging rehearsals never block a PR; add `ci:package` to run them. A new push cancels the superseded run. |
| Master push | The selected lanes that also run on push: the Research web, Rust and platform lanes and `auth` run only on pull requests, because `frontend-tests.yml`, `rust-tests.yml`, `web-build.yml`, the signed builds and `auth-pages-ci.yml` cover master. Agent inputs add the full Agent suites on Linux and macOS. Also the signed builds (Agent: Prod only), the release-evidence workflows and CodeQL. Never cancelled. |
| Nightly | `ci.yml` runs its nightly lanes on the master tip, Rust advisories, packaging rehearsals and the Agent's native Windows suites included, which also refreshes their caches. `nightly.yml` runs the Bun advisories and the SQLCipher provider suite on every OS. Failures and [quarantined lanes](#quarantine) open or update one `ci-nightly` issue, whose report waits up to two hours for `ci.yml`'s run to finish; a green night closes it. CodeQL also runs nightly, and `agent-desktop-build.yml` builds, signs and verifies the Agent Dev packages; the `ci-nightly` issue covers neither of these. The crons say 04:17 (`ci.yml`), 05:28 (CodeQL), 05:47 (`nightly.yml`) and 10:37 UTC (Agent Dev), but GitHub starts scheduled runs hours late, so nothing may depend on exact times. |
| On demand | PR labels (below), or a `ci.yml` dispatch with comma-separated lane ids or `all`. Dispatch runs never save caches. `os-eif` cannot be dispatched: an id that is not a dispatch lane, a typo included, makes the run fall back to every dispatch lane. |

`master` requires `ci-ok` and auto-merge is on. Branches need not be up to
date, so master's own push run is the check for conflicts between pull
requests that each passed alone.

## Labels

Most PRs need no label. Every lane a PR waits on by default takes a few
minutes; builds that take longer (desktop bundles, packaging rehearsals, the
full native suites) run only by label, nightly, or on master. Add a label when
a change could break something only those builds reveal, for example:

- `ci:windows` for path, filesystem, process or terminal code that behaves differently on Windows;
- `ci:package` for changes to packaging or release scripts, the flakes or toolchains, or what ships in a bundle (assets, resources, entitlements, updater metadata);
- `ci:ios` or `ci:android` for Research changes that reach native mobile code or mobile-only plugins;
- `ci:full` for toolchain, lockfile-wide or other risky cross-cutting changes before merge.

Add the label, then push or use **Re-run all jobs**: `plan` reads labels when
it runs, and re-running only the failed jobs reuses the earlier plan. Every
later push runs the labelled lanes again until the label is removed.

| Label | Adds |
| --- | --- |
| `ci:full` | Every lane except the OpenSecret EIF candidate |
| `ci:package` | Unsigned Agent and Research desktop packages on every OS, and the unsigned Research Dev app |
| `ci:windows` | The Agent's Windows Clippy, native updater tests and full Windows suite |
| `ci:ios`, `ci:android` | The Research iOS simulator or Android build |
| `ci:research-dev-macos` | The unsigned Research Dev app |
| `ci:eif` | The OpenSecret EIF candidate (same-repository PRs to `master` that edit no PCR approval) |

## Run a lane locally

`just lane <id>` (the same as `python3 scripts/ci/plan.py run <id>`) runs the
lane's command in its pinned Nix shell, exactly as `ci.yml` does. macOS lanes
need macOS and use its selected Xcode (CI pins it with
`scripts/ci/select-xcode.py`; only the packaging rehearsal also needs its Metal
Toolchain), Windows lanes need Windows with the toolchain
from `.github/actions/setup-agent-windows`, `os-sdk-e2e` needs PostgreSQL on
`127.0.0.1:5432` with the credentials in `scripts/ci/opensecret-sdk-e2e.sh`,
`os-helpers` needs Docker that builds `linux/arm64`, and `os-eif` needs Linux
on ARM64 and `EIF_MODE=dev` or `prod`. Before pushing a workflow, CI-script,
flake or documentation change, run `just lane repo` (every root flake check):
docs select no lane, so a docs-only pull request does not run the check that
docs name only existing workflows. The
[pre-commit hook](repository-workflows.md#pre-commit-hook) runs the fast part
of the lanes your staged files select.

## Add or change a lane

1. Add or edit one `Lane(...)` in `scripts/ci/lanes.py`. Set `hook` when a
   component pre-commit hook runs the same checks.
2. Add a `scripts/ci/test_plan.py` case showing which paths select it.
3. A lane in the generic `nix` or `windows` matrix needs nothing else. Add a
   special `ci.yml` job only when its steps cannot be generic (services, OIDC,
   artifacts, or a cache another workflow writes). The job id is the lane's
   `job`, its real work is a script or recipe, and `ci-ok` needs it.
4. A new top-level directory, or a new one under `services/`, is unknown to
   the OpenSecret selector, which then runs every OpenSecret lane for it.
   Adding its prefix to the selector's `KNOWN_INDEPENDENT_PREFIXES` is an
   OpenSecret change: it also changes what `opensecret-eif.yml` checks.
5. Run `python3 scripts/ci/plan.py lint` (every glob matches a tracked file,
   every tracked file is owned or inert, the `ci.yml` jobs and their runners
   and timeouts match the table) and `just lane repo`. A new runner label also
   goes in `.github/actionlint.yaml`.

## Rules

- Never add `paths:` to `ci.yml`; `ci-ok` must report on every pull request.
- Without labels, every pull request must reach `ci-ok` well under 10
  minutes. A lane slower than about 8 minutes stays off the pull-request
  path: it runs only by label, nightly, or as master's real builds. Check its
  warm duration before selecting it by path on pull requests.
- Lanes never save a cache on a pull request; only master pushes and the
  nightly run do, into the store of the lane's own runner vendor:
  Blacksmith's, or GitHub's for the Agent lanes on GitHub's Windows image.
  The EIF jobs are the exceptions: on pull requests they may write
  branch-scoped Actions-cache entries that master cannot read, and
  same-repository EIF builds push to FlakeHub
  ([caches](repository-workflows.md#caches)). A lane cache must match the
  lane's toolchain exactly; after a lockfile change, a run restores the
  newest one by prefix, and master rebuilds the lane's target from scratch
  before it saves.
- Never cancel master, signed, evidence or nightly runs.
- Never edit the release or publisher workflows, signing environments or
  OpenSecret approval gating to clear a check. A failing contract test means
  the workflow change is wrong, not the contract.
- Keep each lane's work in a script or recipe, so it runs outside GitHub
  Actions.

## Quarantine

To stop a broken or flaky lane from blocking every pull request while it is
fixed, set the repository variable `CI_QUARANTINE` to its lane id
(comma-separated for several); no pull request is needed. A quarantined lane
is still selected and runs, and its own check shows its real result, but
`ci-ok` reports its failure as a warning instead of failing. In the `nix` and
`windows` matrices, the run's job results tell its failure apart from its
neighbours'. An id that names no lane fails `plan`, so a typo cannot
quarantine nothing; so does `repo`, which enforces the workflow policy and
release contracts. The nightly report lists every active quarantine; clear the
variable when the lane is fixed.

## Runners

The `Runner` constants at the top of `scripts/ci/lanes.py` assign every lane
its runner; `plan.py lint` keeps `ci.yml`'s special jobs in step. Standard
GitHub-hosted runners are free for this public repository; Blacksmith and the
org's 16-core GitHub runner are billed by the minute.

| Runner | Lanes | Why |
| --- | --- | --- |
| `blacksmith-8vcpu-ubuntu-2404`, `blacksmith-6vcpu-macos-27`, `blacksmith-8vcpu-windows-2025` | Most compiling lanes, for example Research Rust, OpenSecret Rust, every macOS lane and the Agent's Windows Clippy | Fast, with warm caches in Blacksmith's store |
| `blacksmith-32vcpu-ubuntu-2404` | `agent-lint`, `agent-test` | The heaviest cold builds |
| `xcode-27` | `metal-toolchain` | GitHub's Xcode 27 image ships Xcode's Metal Toolchain, which Blacksmith's macOS image lacks and cannot download; it exports it for the macOS packaging rehearsal, the only lane that compiles gpui's shaders. The Agent's other macOS lanes build with runtime shaders. It is a public-preview label without an SLA; if GitHub retires it, use the label that carries Xcode 27.1 |
| `ubuntu-latest`, `ubuntu-24.04-arm` | Lighter lanes, for example `repo`, SDK, proxy, Research web, services, `deps-*` and `os-pcr` | Free, and fast enough without Blacksmith |
| `windows-2025` | `agent-windows-updater`, `agent-full-windows` | Blacksmith's Windows workspace inherits a foreign write ACE that the updater's storage validation rejects, and the reparse tests need the symlink privilege GitHub's runner grants |
| `windows-11-arm` | `agent-package-windows-arm64` | Native ARM64 compilation and the unsigned installer |
| `maple-windows-2025-16core` | `agent-package-windows`, `research-desktop-windows` (for speed) | Same image and cache store as `windows-2025`, with more cores |
| `blacksmith-16vcpu-ubuntu-2404-arm` | `os-eif` | The release EIF builder's class ([contracts](repository-workflows.md#contracts)) |

During an outage or a queue burst, set the repository variable
`CI_LINUX_RUNNER`, `CI_LINUX_HEAVY_RUNNER`, `CI_MACOS_RUNNER`,
`CI_WINDOWS_RUNNER` or `CI_WINDOWS_GITHUB_RUNNER` to another label; no pull
request is needed. Matrix lanes pick it up when `plan` next runs (**Re-run
all jobs** on a finished run, or the next push); special jobs read it when
they are queued.
`CI_WINDOWS_GITHUB_RUNNER=windows-2025` moves the 16-core lanes to the
standard runner, which uses the same cache store. A runner from the other
vendor never saves a cache. The EIF builder, `os-helpers` and the free
Windows suites have no override. Runners of the signed and release workflows
are in [repository workflows](repository-workflows.md#runners-and-toolchains).
