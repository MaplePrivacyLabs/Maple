# Changing CI

Paths are relative to the repository root. `scripts/ci/lanes.py` and
`scripts/ci/plan.py` are short; read them before a non-trivial change.

## Lane fields

| Field | Meaning |
| --- | --- |
| `id` | Lane id, which is also the check name |
| `paths` | The lane's own inputs. They select it in CI, and the hook selects its `hook` component from these alone |
| `also` | CI-only fan-out to consumers (for example, the Rust SDK selects Agent lanes). The hook ignores it |
| `pr_paths` | `None`: PRs use `paths + also`. `()`: never path-selected on PRs (label only), while push and nightly still use `paths + also`. A tuple: a narrower PR selector |
| `labels` | PR labels that add the lane (default `ci:full`) |
| `events` | Events the lane is active for (`GATE`, `PR_SIDE`, `PR_AND_NIGHTLY`, `NOT_NIGHTLY`, or a custom set) |
| `job` | `nix` or `windows` (the generic matrices), or the id of a special `ci.yml` job |
| `runner` | A `Runner(label, variable)` constant from the top of `lanes.py` |
| `flake`, `workdir`, `run`, `env` | `plan.py run` runs `cd <workdir> && <run>` under `bash -euo pipefail`, inside `nix develop --no-update-lock-file <flake>` when set. On Windows it uses Git Bash from `PATH`, without Nix |
| `cache`, `workspaces`, `cache_files`, `cache_targets` | rust-cache shared key, workspaces, toolchain pins hashed into the key, and whether to cache `target/` (False keeps only the Cargo home) |
| `timeout` | Minutes |
| `gate` | Selected when CI infrastructure changes. Set `False` for slow builds |
| `opensecret` | The `opensecret_change_detection.py` output that selects the lane |
| `submodules` | Recursive submodules with full blobless history (Nix needs `revCount`) |
| `hook` | Pre-commit component that runs the same checks locally |
| `restore` | Read-only restore of a cache another workflow writes |

Event sets:

- `PR_SIDE` lanes never run on master pushes, because other workflows cover
  master: Research web and Rust, Research platform builds, and Auth.
- `NOT_NIGHTLY` is for lanes `nightly.yml` already covers.
- `PR_AND_NIGHTLY` is for slow lanes whose cache the nightly run saves, such
  as packaging and the full Windows suite.

## How selection works

- Globs: `**` spans directories, `*` stays within one segment, and a leading
  `!` excludes paths from the globs listed with it. Matching is full-path.
- **PR diff:** `git diff --no-renames` from the merge base, so a rename also
  selects its old path. **Push diff:** `before..after`. With no usable diff
  (a new branch, a force-push, or a missing SHA), every active lane runs.
- **Inert paths** (`INERT`) select only what the OpenSecret selector routes
  them to. Root and component docs, READMEs, hooks and `.agents/**` are
  inert. Deeper files are not, because the Agent compiles nested `SKILL.md`
  files. A script you add under `.agents/skills/<name>/scripts/` is inert and
  unchecked unless you exclude it from `INERT` and give it a lane, as
  `release-maple/scripts/**` has.
- **Unclassified paths** select every active lane and ignore `pr_paths`, so a
  PR waits on the label-only builds too. `lint` also fails until the path is
  owned or inert.
- **CI infrastructure** (`CI_INFRA`: `ci.yml`, `.github/actions/**`,
  `plan.py`, `lanes.py`) selects every lane with `gate=True`.
- **Labels** add active lanes on `pull_request` only. `plan` reads them
  through `gh api` when it runs, and a read failure fails the PR's plan.
- **Dispatch:** `lanes` is a comma list or `all`. A name that is not active
  for dispatch makes selection fail, and dispatch then runs every dispatch
  lane.
- **Schedule:** every lane active for `schedule`.
- **`os-eif`** is kept only for a `pull_request` with base `master`, a head
  repository equal to this repository, and a usable diff. It is dropped when
  any path edits a PCR approval.

`plan.py explain` calls the same selection with no labels and no `os-eif`
filter. For label behaviour, read the `labels=` fields or run the
`test_plan.py` helpers:

```sh
python3 -c 'import sys; sys.path.insert(0, "scripts/ci"); import plan; print(sorted(plan.select("pull_request", ["docs/x.md"], ["ci:package"])))'
```

## Add or change a matrix lane

1. Add the `Lane(...)` to `LANES`, next to its component's lanes.
2. Put the command in a script under `scripts/ci/` or a component `just`
   recipe. `run` should be one short line.
3. Choose `paths` from what the command reads. Use `also` only for consumers
   that build this lane's inputs from the tree.
4. Choose the PR exposure:
   - default for a fast lane;
   - `pr_paths=()` plus a label for anything slower than about 8 minutes;
   - `gate=False` if a CI edit should not run it.
5. Add a `test_plan.py` case. Use the helpers `selected(paths, event,
   labels)` and `active(event)`. Assert the PR and push selection, the
   labels, and nightly membership. Follow the existing classes, for example
   `AgentLaneTests`.
6. If the lane has a `hook`, add a `hook_selected <name>` branch to
   `.githooks/pre-commit`. `test_plan.py` checks that one exists.
7. Run `plan.py lint`, `just lane <id>` and `just lane repo`.

## Add or change a special job

A special job is for services, OIDC, artifacts, or another workflow's cache.
Its id is the lane's `job`. `plan.py lint` and policy tests 7 to 9 check the
following:

- `if: fromJSON(needs.plan.outputs.lanes)['<id>']`, plus any extra guard the
  planner also applies. For `os-eif`, change the job's `if:` and the
  planner's candidate rule together.
- `runs-on` exactly `${{ vars.<VARIABLE> || '<label>' }}` when the runner has
  an override variable, otherwise the plain label. `timeout-minutes` equals
  the lane's `timeout`.
- Add it to `ci-ok`'s `needs`.
- Declare a new runner label in `.github/actionlint.yaml`.
- Put the work in `plan.py run <id>` or a script, not inline YAML.
- Guard cache saves to master `push` or `schedule` and to the home vendor
  (`startsWith(runner.name, 'blacksmith')`, or its negation for a GitHub
  home). Restore-only caches use `actions/cache/restore`.
- Check out with `persist-credentials: false`. Any full history must use
  `filter: blob:none`; full checkouts on Blacksmith can otherwise take
  minutes.

Never rename the matrix jobs' `name: ${{ matrix.id }}`. The quarantine
verdict maps job names to lane ids.

## Runners and overrides

The `Runner` constants at the top of `lanes.py` are authoritative.
[CI runners](../../../../docs/ci.md#runners) explains each choice.

| Constant | Override variable |
| --- | --- |
| `LINUX` (Blacksmith 8 vCPU) | `CI_LINUX_RUNNER` |
| `LINUX_HEAVY` (Blacksmith 32 vCPU) | `CI_LINUX_HEAVY_RUNNER` |
| `MACOS` (Blacksmith macOS) | `CI_MACOS_RUNNER` |
| `WINDOWS` (Blacksmith Windows) | `CI_WINDOWS_RUNNER` |
| `GITHUB_WINDOWS` (`maple-windows-2025-16core`) | `CI_WINDOWS_GITHUB_RUNNER`, read only by special jobs |
| `GITHUB`, `GITHUB_WINDOWS_FREE`, `EIF_BUILDER`, inline `ubuntu-24.04-arm` | none |

- Matrix lanes resolve their runner when `plan` runs. A new matrix variable
  must be passed in the `plan` step's `env`; `lint` checks this.
- An override to the other vendor restores what it can and saves nothing.
  Failover runners are often slower, while timeouts are sized for the home
  runner.
- Never move signing, attestation or verification jobs, the updater tests or
  install smokes to Blacksmith, and never move one EIF builder alone.

## Caches

- Lane caches are rust-cache with `prefix-key: ci-v2`. Bumping the prefix or
  a lane's `cache` name, editing `cache_files`, or changing any
  `CARGO*`/`RUST*` variable in the step's environment (including `ci.yml`'s
  top-level `env`) makes the next run cold. Its PR stays cold on every push,
  because PRs never save.
- Only master `push` and `schedule` runs save, through `save-if` and
  `matrix.cache_save` (the vendor rule).
- `ci.yml` may read another workflow's cache only with
  `actions/cache/restore`, and only for a key a push-only workflow saves.
- Never write a release-reserved prefix (`<os>-sccache-`, `xcode-cache-`,
  `maple-dev-xcode`, `release-onnxruntime-ios-`, `<os>-bun-`, `<os>-gradle-`,
  `ci-v1-agent-release-`) from `ci.yml` or a PR job.
- Turn off implicit saves on PR paths: setup-bun `no-cache: true`, Trivy
  `cache: "false"`, and no Magic Nix Cache.
- After a lockfile change, master's first save deletes the lane's target
  first, so that master run is slower than its PR.

## Timeouts

Size `timeout` for a cold run on the home runner, with headroom. Agent lint
and test use 40 minutes although warm runs take about 3. A timeout protects
against hangs; the 10-minute rule decides PR exposure. `test_7` requires a
timeout on every non-protected job.

## The pre-commit hook

`./setup-hooks.sh` sets `core.hooksPath` to `.githooks`. The hook runs
`plan.py hook --staged`, then each selected component's own hook inside its
flake: formatting, lint, type checks and unit tests only. It uses only
`paths`, never `also` or `CI_INFRA`. Preview it with
`printf '%s\n' <path> | python3 scripts/ci/plan.py hook`. The environment
switches are `MAPLE_HOOK_SKIP`, `MAPLE_HOOK_FULL`, `MAPLE_HOOK_NO_NIX` and
`MAPLE_HOOK_ONLY`. See [the hook](../../../../docs/repository-workflows.md#pre-commit-hook).

## Guard tests

All run in the `repo` lane, through the flake checks `ci-scripts`,
`workflows` and `release-gates`.

- **`test_plan.py`** covers selection, labels, events, the verdict,
  quarantine, cache rules, the hook mapping and `plan.py lint`.
- **`test_workflow_policy.py`** holds rules for every workflow:
  1. no `pull_request_target`, and a fixed set of PR workflows;
  2. actions pinned to commits;
  3. no persisted checkout credentials;
  4. read-only workflow permissions;
  5. secrets only in their protected environment;
  6. a pinned, verified Nix install;
  7. a timeout on every job;
  8. `ci-ok` always reports and needs every job;
  9. declared runner labels.

  It also checks that PRs never save caches and that only the listed jobs
  write GitHub's store.
- **`test_contracts.py`** imports the code on the other side of each seam.

| Contract | Protects | If it fails |
| --- | --- | --- |
| C1 | One `Release` workflow; listeners check their producer's success, repository and event; release jobs gated | You touched a release trigger or listener. Revert it |
| C2 | The Agent publisher and signature probe trust `agent-desktop-build.yml` jobs, artifacts, runners and events | You renamed or moved a signed Agent job. Revert it |
| C3 | Release evidence jobs run on master pushes and never cancel | You changed an evidence workflow's trigger, job or concurrency |
| C4 | Release cache ownership, exact Agent release keys, lane-family isolation, Dev TestFlight build numbers | You wrote or overlapped a release cache key |
| C5 | The EIF event gate, mismatch semantics, OIDC and FlakeHub jobs, one builder class, a manual EIF release | You changed an EIF job, runner or guard |
| C6 | Each protected environment has fixed consumer jobs; `windows-signing` is GitHub-hosted | You added, moved or renamed an `environment:` |
| C7 | Docs name only existing workflow files and display names | A doc names a missing `.yml` file, or a backticked or bold name before "workflow" that is not a display name |

C7 reads `docs/**`, every component `docs/`, `.agents/**`, and every README,
AGENTS and CLAUDE file. Docs are inert, so it runs only when `repo` is
selected or nightly; run it yourself after doc edits.

## Other workflows

- Only `ci.yml`, `opensecret-eif.yml`, `pages-preview-build.yml`,
  `auth-pages-ci.yml` and `proxy-rust.yml` may run on pull requests
  (`PULL_REQUEST_WORKFLOWS`). Adding one is an owner decision.
- The release-evidence workflows (`frontend-tests.yml`, `rust-tests.yml`,
  `web-build.yml`, `desktop-build.yml`, `android-build.yml`,
  `mobile-build.yml`, `codeql.yml`) feed the release preflight. Do not fold
  them into `ci.yml`, rename their jobs, or change their `paths:` without the
  release owner.
- `pages-preview-build.yml` and `auth-pages-ci.yml` are producers that
  `pages-publish.yml` and `auth-pages-dev-publish.yml` trust by name and
  artifact. They overlap `research-web` and `auth` on purpose: the publishers
  need their artifacts, while `ci-ok` needs the lanes.
- `rust-tests.yml` is the only writer of `research-rust`'s cache.
- The Nix version is pinned in every workflow, protected ones included. A Nix
  bump therefore touches release files; make it a separate PR for the owner.

## OpenSecret changes

- Never edit `pcr*.json` or `opensecret_change_detection.py` to clear a
  check. A new top-level directory makes the selector run every OpenSecret
  lane. Adding it to the selector's `KNOWN_INDEPENDENT_PREFIXES` is an
  OpenSecret change, reviewed as one.
- For EIF checkout, Nix, cache or runner changes, prove that narHash,
  drvPaths and PCRs match the last master `eif-trusted` run, on fresh hosted
  runners. Get the PR evidence with `ci:eif` and **Re-run all jobs**.
- Read [CI approval checks](../../../../services/opensecret/docs/nitro-deploy.md#ci-approval-checks)
  and the [cache policy](../../../../services/opensecret/docs/nitro-deploy.md#binary-caches-and-cold-run-validation)
  first.

## Run lanes locally

`just lane <id>` runs exactly what CI runs. Limits:

- macOS lanes need macOS and the pinned Xcode (`scripts/ci/select-xcode.py`).
- Windows lanes need Windows with the toolchain from
  `.github/actions/setup-agent-windows`.
- `os-sdk-e2e` needs PostgreSQL on `127.0.0.1:5432` with the credentials in
  `scripts/ci/opensecret-sdk-e2e.sh`.
- `os-helpers` needs Docker that can build `linux/arm64`.
- `os-eif` needs Linux ARM64 and `EIF_MODE=dev` or `prod`.

`just lane repo` runs every root flake check. To run one, use
`nix build --no-link --no-update-lock-file ".#checks.$(nix eval --impure --raw --expr builtins.currentSystem).<name>"`,
where `<name>` is `ci-scripts`, `workflows`, `release-gates`, `pages` or
`pcr-compatibility`.
