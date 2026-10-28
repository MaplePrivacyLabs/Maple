# Why Maple CI has this shape

Each section gives a design choice, the reason for it, and the code that
enforces it. Read the matching section before you argue for changing a rule.

## One workflow, one planner, one table

A check is one `Lane(...)` in `scripts/ci/lanes.py`. Three readers share that
table:

- `plan.py github`, which is `ci.yml`'s `plan` job;
- `plan.py hook`, which is `.githooks/pre-commit`;
- `just lane <id>`, which is `plan.py run`.

CI, the hook and local runs therefore execute the same command string and
cannot drift. Separate workflows with their own `paths:` filters and change
selectors drift apart and multiply check rows. A planner can do what
`paths:` filters cannot:

- An unknown path runs every lane.
- A selection error fails closed on a PR. On master, nightly and dispatch it
  runs everything.
- Labels are read live.
- Every choice is explained in the job summary.

Special jobs exist only where a generic step list cannot do the work:
services (`os-sdk-e2e`), OIDC (`os-eif`), artifacts (packaging, iOS, desktop)
or another workflow's cache (`research-rust`). Unselected matrix lanes never
appear as check rows. Keeping each lane's work in a script or recipe means it
also runs outside GitHub Actions.

## One required check

The lanes differ per PR, so no lane can be required by name. `ci.yml` has no
`paths:` filter, so `ci-ok` reports on every PR and never stalls as "waiting
for status". The verdict is "selected means success". A skipped result passes
only for an unselected job, so a lane skipped by a broken `if:` cannot pass
silently. `ci-ok` runs `if: always()` and needs every other job.
`test_8_ci_always_reports_one_verdict` and `plan.py lint` enforce this.

## Merging without a queue or an up-to-date rule

The `master` branch rules require `ci-ok` and no other status check.
Auto-merge is on. There is no merge queue, and branches need not be up to
date. Either would add a
second full `ci-ok` run to every merge and fight stacked PRs. Master instead
re-verifies every push, and the team fixes forward. `ci.yml` already handles
`merge_group`, so a queue is a settings change if it is ever needed.

## The 10-minute rule

Every PR must reach `ci-ok` well under 10 minutes. A lane is path-selected on
PRs only if its warm runs finish in about 8 minutes. Packaging rehearsals,
desktop bundles, the full native suites and Windows harnesses run only by
label, nightly, or as master's real signed builds. Master's
`agent-desktop-build.yml`, `desktop-build.yml` and the other signed workflows
already build the real artifacts on every relevant push. A PR rehearsal adds
waiting, not coverage.

No code enforces this rule; there is no timing guard. Slow lanes crept back
before, one path selector at a time, until PRs waited 35 to 45 minutes. Check
a lane's warm duration (the run history of its check row) before you
path-select it.

`test_plan.py` pins the current choices. Packaging is label-only, and the
full suites run on master, nightly and by label. Research iOS and Android are
still path-selected by their native inputs, and `os-eif` by measured
OpenSecret inputs. They are slower, but they are the only PR proof for those
inputs.

## Pull requests never save caches

- A PR author can edit the YAML, so a PR-written cache could poison what
  master or a release later restores. Ref scoping is the real boundary:
  Blacksmith's Actions-cache branch protection must stay on. "PRs never save"
  is hygiene on top.
- PR-scoped entries once filled GitHub's cache store and froze every save.
- GitHub's store evicts the least recently used entries, which could push out
  the caches releases restore.
- A PR cache serves only that PR's next push. Master caches serve every PR.

`save-if` in `ci.yml` allows only master `push` and `schedule`. The policy test
`test_pull_requests_never_save_caches…` also checks dispatch and queue runs,
and actions that save implicitly (setup-bun, Magic Nix Cache, Trivy). There
are two deliberate exceptions, and neither writes the Actions cache that
master reads:

- `opensecret-eif.yml`'s unprivileged `eif` job writes only its own
  branch-scoped Actions cache.
- The same-repository PR `os-eif` candidate pushes to FlakeHub, a Nix binary
  cache, through job-scoped OIDC.

## Master runs are never cancelled

Master plans each push from `before..after`. A cancelled run means that diff
is never verified, because the next push checks only its own diff. Master runs
also save the caches and produce release evidence. The release preflight
never prefers an older green run over a newer failed or pending one, and
contract C3 forbids cancelling the evidence workflows. The signed Agent
workflow uses a per-run group, so pending packages are never discarded; its
run number is the build number.

## No skip tokens

`[skip ci]` on a master commit means that commit's diff is never planned. A
PR title becomes the squash commit message, so the rule covers titles too.
A skipped PR run also leaves the required `ci-ok` unreported. No code guards
this; `AGENTS.md` states the rule.

## Runners

Standard GitHub-hosted runners are free for this public repository.
Blacksmith and the org's 16-core GitHub Windows runner are billed per minute.

| Work | Runner class | Why |
| --- | --- | --- |
| Compiles people wait on: Agent lint and test, macOS lanes, Windows Clippy, Research Rust, OpenSecret Rust | Blacksmith | Fast, and caches stay warm in Blacksmith's store. The two heaviest Agent lanes use the 32-vCPU class, about twice as fast cold as 8 vCPU |
| `plan`, `ci-ok`, `repo`, SDK, proxy, web, services, `deps-*`, `os-pcr`, `os-flake-check`, CodeQL | Free GitHub Linux | Light work, or nobody waits on it |
| Agent native Windows suites (`agent-windows-updater`, `agent-full-windows`) | Free `windows-2025` | Blacksmith's Windows workspace has an inherited write ACE that the updater's storage validation rejects. The reparse tests need the symlink privilege GitHub grants. 16 cores only add queueing to a short suite |
| `agent-package-windows`, `research-desktop-windows`, release `build-windows` | `maple-windows-2025-16core` | Install smokes need GitHub's image; the bundles gain from cores |
| Agent master Windows build | Blacksmith Windows | The fat-LTO link is single-threaded, so 16 cores are no faster |
| Attestation, signing, verification, TestFlight submission | GitHub-hosted | The Agent publisher verifies attestations with `--deny-self-hosted-runners`, and Blacksmith counts as self-hosted. `windows-signing`'s Azure trust names GitHub-hosted runners (C6). Signing and verify jobs restore no cache |
| `os-helpers`, proxy arm64 image | Free `ubuntu-24.04-arm` | No heavy compile |
| Every EIF builder that uses FlakeHub | `blacksmith-16vcpu-ubuntu-2404-arm` | One class for PR candidates, master checks and the release build (C5). `opensecret-eif.yml`'s unprivileged fork job stays on `ubuntu-24.04-arm` |

The override variables (`CI_LINUX_RUNNER`, `CI_LINUX_HEAVY_RUNNER`,
`CI_MACOS_RUNNER`, `CI_WINDOWS_RUNNER`, `CI_WINDOWS_GITHUB_RUNNER`) move lanes
in an outage or a queue burst without a PR. They are unset by default. The
EIF builder, `os-helpers` and the free Windows suites have none on purpose.
C5 pins the EIF builder; the others have no better fallback.

## Cache stores and keys

- **Vendor stores.** Blacksmith runners use Blacksmith's Actions-cache store,
  and GitHub runners use GitHub's. Neither reads the other's. A lane
  therefore saves only into its home runner's store. During a fail-over it
  restores what it can and saves nothing, so it never plants unreadable
  entries that use up quota. GitHub's store also holds what releases restore
  and evicts by recent use, so the jobs allowed to write it are pinned
  (`GITHUB_STORE_WRITERS`, `GITHUB_STORE_LANES`). Moving a job across vendors
  orphans its cache, and its next run is cold.
- **Lane caches (`ci-v2`).** The key is the lane's cache name, a hash of its
  toolchain pins (`cache_files`), and rust-cache's own hash of the runner's
  rustc and `CARGO*`/`RUST*` variables. All of these must match exactly.
  rust-cache adds the lockfile hash, which may differ: after a lockfile
  change, the newest entry of the same lane and toolchain restores by prefix.
  A saving master run without an exact hit first deletes the lane's target
  directory, so each saved cache holds one dependency graph.
- **`agent-cargo-home`.** `agent-supply-chain` and `agent-provider` only
  resolve the Agent graph. They share a Cargo-home-only cache (registry and
  git sources, no target), so they stop re-cloning large git dependencies.
- **Release caches stay exact.** The signed Agent builders restore and save
  only `ci-v1-agent-release-<os>-<hash of both lockfiles>`, with no
  `restore-keys`. A prefix fallback could carry a removed crate's build output
  into a signed binary. Only Prod pushes save it; the nightly Dev and dispatch
  builds only restore it. No `ci.yml` lane reads it. A `Cargo.lock` change,
  version bumps included, makes the next master Agent build fully cold. That
  cost is known and accepted.
- **Reserved prefixes.** `release.yml` restores `<os>-sccache-`,
  `xcode-cache-`, `release-onnxruntime-ios-`, `<os>-bun-` and `<os>-gradle-`
  by prefix. Only the signed master workflows write them. `ci.yml` only
  restores keys that master saves. No lane family shares a prefix with a
  release family, or a new key would hijack the release's fallback (C4).

## Fat LTO and the packaging rehearsals

The owner chose fat LTO with one codegen unit for Agent release builds
(`apps/maple-agent/Cargo.toml`), because download size matters. Thin LTO
measured clearly larger binaries. Do not re-pitch it. The consequence is a
master Agent build of roughly 25 minutes, mostly single-threaded compile and
link, which bigger runners do not speed up. The `ci:package` rehearsals
deliberately build thin LTO (`FAST_RELEASE` in `lanes.py`) into their own
`ci-v1-agent-package-*` caches. They prove the packaging, not the optimizer.
Only the nightly master run saves those caches.

## Agent Dev builds run nightly

`agent-desktop-build.yml` builds Prod on master pushes, Dev nightly at 10:37
UTC, and both on dispatch. Prod no longer waits on, or gets skipped by, a
failed Dev leg, and each push saves a full Dev build set. The nightly run can
never be published, because the publisher accepts only `push` and
`workflow_dispatch` runs. The hour avoids the other scheduled runs and the
top of the hour. Research iOS Dev TestFlight stays on push.

## CodeQL only has to be "not red"

The release preflight needs the newest finished CodeQL scan of the release
commit, or an ancestor, to be green. A running or cancelled scan neither
helps nor blocks; a red one blocks. This keeps the security signal without
making every release wait for a fresh scan. CodeQL runs on master pushes,
nightly and on dispatch, grouped per SHA and never cancelled. It stays on the
free runner because nothing waits on it. It is not part of `ci-ok`.

## Dependency audits run on their own inputs

The `deps-*` cargo-deny lanes run when their manifest, lockfile or
`deny.toml` changes, with `ci:full`, and nightly. CI edits never select them.
A newly published advisory in an unrelated crate must not turn unrelated PRs
red; the nightly run reports it. `agent-supply-chain` and `sdk-ts` still
audit whenever they are selected.

## OpenSecret EIF checks

- **One runner class.** The PR candidate (`os-eif`), the trusted master
  comparison (`opensecret-eif.yml`'s `eif-trusted`) and the release build
  (`opensecret-eif-release.yml`) all fill or read the FlakeHub cache. Keeping
  them on one class means no untested builder variable reaches store paths
  that feed PCR reproducibility. Move all three together or none. C5 and
  `plan.py lint` enforce this.
- **Strict only where approvals are edited.** A source change alters PCRs by
  design, and approval is a manual release step. On master a mismatch (exit
  code 3) is an "approval pending" notice with the built PCRs. A PR that
  edits `pcrDev*` or `pcrProd*` claims those measurements, so a mismatch for
  that mode fails. Every other non-zero exit fails, so build errors cannot
  hide. Approval status is not part of `ci-ok`.
- **`os-eif` runs only for same-repository PRs to `master` that edit no
  approval.** The job mints OIDC to write FlakeHub, which fork code must never
  get. When a PR edits approvals, `opensecret-eif.yml` compares instead. The
  job's `if:` repeats the planner's rule; change both together.
- **The selector is imported verbatim.** `opensecret_change_detection.py`
  routes every changed path, docs included. Changing its prefixes changes
  what `opensecret-eif.yml` checks, so treat it as an OpenSecret change.
- **Prove reproducibility, not just green.** For any change to an EIF
  builder's checkout, Nix or flake inputs, show that narHash, drvPaths and
  PCRs match the last master `eif-trusted` run. Check on fresh hosted runners,
  not a warm local Nix store. A master mismatch is only a notice, so CI would
  not catch the drift for you.

## Tests: behaviour and contracts, not text

Tests that assert a script's or workflow's text break on harmless renames and
rewording and catch no behaviour. A hand-copied list of required test names
breaks on the next rename. Test
what the planner selects, or the contract another file consumes:
`test_contracts.py` imports `publisher.py`, `ci_evidence.py` and the
signature probe rather than copying their rules. Derive any list a test
needs.

The policy and contract tests do keep a few deliberate security allowlists:
`PULL_REQUEST_WORKFLOWS`, `GITHUB_STORE_WRITERS`, `GITHUB_STORE_LANES`,
`OIDC_JOBS`, `FLAKEHUB_JOBS`, `CONSUMERS` and `ENVIRONMENTS`. They are not
hand-copied lists. Adding an entry widens a trust boundary, so it is an owner
decision.

## Quarantine

`CI_QUARANTINE` (a repository variable) keeps a broken lane running and
visible without failing `ci-ok`. It exists for emergencies; do not reach for
it to merge a change. Fix the lane, or ask the owner. `repo` cannot be
quarantined, because it enforces the policy and contract tests. See
[CI quarantine](../../../../docs/ci.md#quarantine).
