# CI

`.github/workflows/ci.yml` runs every check a pull request needs. Its one
verdict, `ci-ok`, is the required status check: it passes when every lane the
planner selected succeeded, and an unselected lane may only be skipped. Signed
builds, publication, caches and the contracts CI keeps are in
[repository workflows](repository-workflows.md).

## How lanes are chosen

A lane is one check, defined once in `scripts/ci/lanes.py`: its own paths, the
consumers it fans out to (`also`), events, labels, runner, Nix shell, command
and timeout. `scripts/ci/plan.py` (the `plan` job) selects lanes from the diff:

- A pull request is judged on its own diff from the merge base with its base
  branch, so each layer of a stack gets its own `ci-ok`. A merge-queue group
  uses its base and head; a master push uses `before..after`.
- Docs, guides and component hooks select no lane but those the OpenSecret
  selector routes them to (below). A path that no lane owns selects every
  lane. Editing `ci.yml`, the planner or `.github/actions/` selects every gate
  lane.
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
```

## Tiers

| When | What runs |
| --- | --- |
| Pull request, merge queue | Selected lanes, each a few minutes; the iOS and Android builds when their native inputs change. Desktop bundles and packaging rehearsals never block a PR; add `ci:package` to run them. A new push cancels the superseded run. |
| Master push | The selected lanes again (for Agent inputs, also the full Agent suites on Linux and macOS), the signed builds (Agent: Prod only), the release-evidence workflows and CodeQL. Never cancelled. |
| Nightly | `ci.yml` runs its nightly lanes on the master tip, Rust advisories and the Agent's native Windows suites included, which also refreshes their caches. `nightly.yml` runs the Bun advisories and the SQLCipher provider suite on every OS. Failures and [quarantined lanes](#quarantine) open or update one `ci-nightly` issue; a green night closes it. Separately, at 10:37 UTC, `agent-desktop-build.yml` builds, signs and verifies the Agent Dev packages; the `ci-nightly` issue does not cover it. |
| On demand | PR labels (below), or a `ci.yml` dispatch with comma-separated lane ids or `all`. |

## Labels

Most PRs need no label. Every lane a PR waits on by default takes a few
minutes; builds that take longer (desktop bundles, packaging rehearsals, the
full native suites) run only by label, nightly, or on master. Add a label when
a change could break something only those builds reveal, for example:

- `ci:windows` for path, filesystem, process or terminal code that behaves differently on Windows;
- `ci:package` for changes to packaging or release scripts, the flakes or toolchains, or what ships in a bundle (assets, resources, entitlements, updater metadata);
- `ci:ios` or `ci:android` for Research changes that reach native mobile code or mobile-only plugins;
- `ci:full` for toolchain, lockfile-wide or other risky cross-cutting changes before merge.

Add the label, then re-run the CI workflow (or push): `plan` reads labels when it runs.

| Label | Adds |
| --- | --- |
| `ci:full` | Every lane except the OpenSecret EIF candidate |
| `ci:package` | Unsigned Agent and Research desktop packages on every OS |
| `ci:windows` | The Agent's Windows Clippy, native updater tests and full Windows suite |
| `ci:ios`, `ci:android` | The Research iOS simulator or Android build |
| `ci:research-dev-macos` | The unsigned Research Dev app |
| `ci:eif` | The OpenSecret EIF candidate (same-repository PRs to `master` that edit no PCR approval) |

## Run a lane locally

`just lane <id>` (the same as `python3 scripts/ci/plan.py run <id>`) runs the
lane's command in its pinned Nix shell, exactly as `ci.yml` does. macOS lanes
need macOS and use its selected Xcode (CI pins it with
`scripts/ci/select-xcode.py`), Windows lanes need Windows with the toolchain
from `.github/actions/setup-agent-windows`, `os-sdk-e2e` needs a local PostgreSQL
and `os-eif` needs Linux on ARM64. Before pushing a workflow, CI-script or
flake change, run `just lane repo` (every root flake check). The
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
- Pull requests never save caches (one branch-scoped exception, outside
  `ci.yml`, is listed under [caches](repository-workflows.md#caches)). Master
  and nightly runs save lane caches into the store of the lane's own runner
  vendor: Blacksmith's, or GitHub's for the Agent lanes on GitHub's Windows
  image. A lane cache must match the lane's toolchain exactly; after a
  lockfile change, a run restores the newest one by prefix, and master
  rebuilds the lane's target from scratch before it saves.
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

Lanes use Blacksmith runners or GitHub-hosted runners; the runner
constants are at the top of `scripts/ci/lanes.py`. During an outage, set the
repository variable `CI_LINUX_RUNNER`, `CI_LINUX_HEAVY_RUNNER`,
`CI_MACOS_RUNNER` or `CI_WINDOWS_RUNNER` to another label; no pull request is
needed. `agent-package-windows`, which needs GitHub's image, and, for speed,
`research-desktop-windows` run on the org's 16-core GitHub-hosted runner
`maple-windows-2025-16core` (billed by the minute);
`CI_WINDOWS_GITHUB_RUNNER=windows-2025` moves them back to the standard runner,
which uses the same cache store. The native Windows test suites,
`agent-windows-updater` and `agent-full-windows` (nightly, or by label), use
the free standard `windows-2025`. A runner from the other vendor never
saves a cache.
