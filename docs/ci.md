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
| Pull request, merge queue | Selected lanes, including unsigned platform and package rehearsals when their native inputs change. A new push cancels the superseded run. |
| Master push | The selected lanes again (for Agent inputs, also the full Agent suites on Linux, macOS and Windows), the signed builds, the release-evidence workflows and CodeQL. Never cancelled. |
| Nightly | `ci.yml` runs its nightly lanes on the master tip, Rust advisories included, which also refreshes their caches. `nightly.yml` runs the Bun advisories and the SQLCipher provider suite on every OS. Failures open or update one `ci-nightly` issue; a green night closes it. |
| On demand | PR labels (below), or a `ci.yml` dispatch with comma-separated lane ids or `all`. |

## Labels

Most PRs need no label: `plan` already adds a platform or packaging lane when
its own files change. Add a label when a change could break something those
paths do not reveal, for example:

- `ci:windows` for path, filesystem, process or terminal code that behaves differently on Windows;
- `ci:package` for changes to what ships in the bundle (assets, resources, entitlements, updater metadata) outside the packaging scripts;
- `ci:ios` or `ci:android` for Research changes that reach native mobile code or mobile-only plugins;
- `ci:full` for toolchain, lockfile-wide or other risky cross-cutting changes before merge.

Add the label, then re-run the CI workflow (or push): `plan` reads labels when it runs.

| Label | Adds |
| --- | --- |
| `ci:full` | Every lane except the OpenSecret EIF candidate |
| `ci:package` | Unsigned Agent and Research desktop packages on every OS |
| `ci:windows` | Agent Windows tests and Clippy, the Windows updater tests, and the Research Windows package |
| `ci:ios`, `ci:android` | The Research iOS simulator or Android build |
| `ci:research-dev-macos` | The unsigned Research Dev app |
| `ci:eif` | The OpenSecret EIF candidate (same-repository PRs to `master` that edit no PCR approval) |

## Run a lane locally

`just lane <id>` (the same as `python3 scripts/ci/plan.py run <id>`) runs the
lane's command in its pinned Nix shell, exactly as `ci.yml` does. macOS lanes
need macOS and use its selected Xcode (CI pins it with
`scripts/ci/select-xcode.py`), Windows lanes need Windows with the toolchain
from `.github/actions/setup-agent-windows` (`agent-windows-updater` also needs
the Actions variables its scripts check), `os-sdk-e2e` needs a local PostgreSQL
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
  image.
- Never cancel master, signed, evidence or nightly runs.
- Never edit the release or publisher workflows, signing environments or
  OpenSecret approval gating to clear a check. A failing contract test means
  the workflow change is wrong, not the contract.
- Keep each lane's work in a script or recipe, so it runs outside GitHub
  Actions.

## Runners

Lanes use Blacksmith runners or GitHub-hosted runners; the runner
constants are at the top of `scripts/ci/lanes.py`. During an outage, set the
repository variable `CI_LINUX_RUNNER`, `CI_LINUX_HEAVY_RUNNER`,
`CI_MACOS_RUNNER` or `CI_WINDOWS_RUNNER` to another label; no pull request is
needed. `CI_WINDOWS_GITHUB_RUNNER` moves the three Agent lanes on GitHub's
`windows-2025` (`agent-full-windows`, `agent-windows-updater`,
`agent-package-windows`), for example to a larger GitHub runner (billed by the
minute), which uses the same cache store. A runner from the other vendor never
saves a cache.
