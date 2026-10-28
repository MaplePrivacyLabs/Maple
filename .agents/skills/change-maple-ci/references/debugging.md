# Debugging red or slow CI

Start with the facts the run already has:

- **`plan`'s job summary** ("Lanes for this …") lists every selected lane and
  its first reason. If a lane is missing or unexpected, the answer is there.
- **`ci-ok`'s log** names each job that did not finish as planned, for
  example `nix (selected): failure`. Matrix entries are named after their
  lanes, so open the failing row with that lane id.
- **`plan.py explain`** reproduces the path selection locally (no labels).

## Re-run semantics

| Action | Re-plans? | Use for |
| --- | --- | --- |
| Push a commit | Yes | Code fixes; also picks up new labels and variables |
| **Re-run all jobs** | Yes: `plan` reads the PR's current labels and repository variables | After adding a label, or changing `CI_QUARANTINE` or a `CI_*_RUNNER` variable. GitHub offers it only on a finished run: cancel a running PR run first, never a master run |
| **Re-run failed jobs** | No: reuses the old `plan` outputs (lanes, matrix runners, quarantine) | Infrastructure flakes in a lane |
| Dispatch `ci.yml` | Its own plan from the `lanes` input | Running lanes on a branch outside a PR. It never saves caches |

Adding a label never starts a run by itself. That is deliberate: a label
event could otherwise post a fresh green `ci-ok` on a commit whose real run
failed. With auto-merge on, a run planned before the label can still pass
`ci-ok` and merge the PR without the labelled lanes; push or cancel it before
it finishes.

A new push to a PR cancels the superseded run, and the new `plan` waits for
the cancellation to finish, so rapid re-pushes each add some delay.

## Failure signatures

| Signature | Cause | Response |
| --- | --- | --- |
| Nix setup fails: `HTTP error 416`, `SSL_read`, or "is required, but there is no substituter" | Transient `cache.nixos.org` error. Setup has no automatic retry | Re-run the failed job |
| "lost communication with the server", or a job that vanished mid-run | Lost runner | Re-run failed jobs. On `agent-desktop-build.yml` (signed builds), ask the release owner first; a re-run in the same run keeps the build number |
| `plan` fails with `lane selection failed` on a PR | A label read through `gh api` failed, or the event payload was unusable | **Re-run all jobs** |
| `plan` fails on every event, master included | `CI_QUARANTINE` names an unknown lane or `repo` | Fix or clear the variable |
| `plan` succeeds but selects every lane | An unclassified path, or no usable diff (new branch, force-push) | Own or mark inert the path in `lanes.py`; `plan.py lint` lists it |
| A dispatch ran every lane | A lane id that is not active for dispatch (`os-eif`, or a typo) | Check ids in `lanes.py`. For `os-eif`, add `ci:eif` to the PR and **Re-run all jobs** |
| The `ci.yml` job you edited did not run | CI edits select gate lanes only; slow special jobs are `gate=False` | Add the lane's label, then **Re-run all jobs** |
| A lane is skipped and `ci-ok` fails with `(selected): skipped` | A special job's `if:` disagrees with the planner | Make the job's `if:` match the planner rule |
| `os-eif` did not run on an OpenSecret PR | Fork, base not `master`, a PCR approval edit, or no EIF input | Expected. `opensecret-eif.yml` compares approvals; `ci:eif` works only on same-repository PRs to `master` |
| `repo` fails `test_workflow_policy` or `test_contracts` | Your change crossed a policy or a release seam | Fix the change; see the contract table in [changing CI](changing-ci.md#guard-tests) |
| `repo` fails C7 on an unrelated PR | An earlier docs-only merge named a missing workflow; docs PRs do not run `repo` | Fix the doc reference |
| `agent-lint` fails `check-rust` | The Agent flake's rustc no longer equals the Windows rustup pin | Bump `AGENT_WINDOWS_RUST` and `setup-agent-windows` together |
| An EIF comparison on master reports a mismatch | Measured inputs changed; approval is a release step | A notice, not a failure. Do not edit approvals |
| Any other failure you cannot explain | Possibly an infrastructure fault | Re-run the failed job once. A repeat is real: fix it, and do not quarantine it to merge |

## Slow runs

- **Cold cache.** The first run after any of these is cold:
  - a lockfile change: a prefix fallback restores the nearest entry (a
    workspace version bump still hits rust-cache lanes exactly, but not
    `research-rust`, `research-android` or the packaging caches);
  - a new cache prefix, lane `cache` name or `cache_files` pin;
  - a change to a `CARGO*` or `RUST*` variable;
  - a runner image with a new preinstalled Rust or MSVC.

  PRs never save, so the PR stays cold until master or the nightly run saves.
  Do not shrink timeouts to warm times.
- **Nightly-only caches.** `agent-full-windows` and the `agent-package-*`
  lanes save only from the 04:17 UTC nightly run. A labelled run right after
  a lockfile change restores by prefix and is slower.
- **Agent master builds.** The exact-key release cache misses when a
  third-party package, a dependency edge or `flake.lock` changes (a version
  bump of workspace or path crates keeps it), and fat LTO bounds the link. The
  master Agent build is slow by design; it does not gate PRs.
- **Queued jobs.** If `agent-lint` or `agent-test` sit queued while other
  Blacksmith jobs start, the 32-vCPU pool is bursting. Set the repository
  variable `CI_LINUX_HEAVY_RUNNER=blacksmith-16vcpu-ubuntu-2404` (same
  vendor, so caches still work) and unset it afterwards. For the 8-vCPU
  lanes, `CI_LINUX_RUNNER=blacksmith-16vcpu-ubuntu-2404` keeps Blacksmith's
  store. In a Blacksmith-wide outage use `ubuntu-latest` (or `macos-26`,
  `windows-2025`): cold, saves nothing, slower.
  - Matrix lanes take their runner from `plan`, so re-plan: cancel a PR run,
    then **Re-run all jobs**. Never cancel a master run; the variable applies
    from the next push. Special jobs read the variable when they are queued.
  - Changing repository variables needs admin rights, so ask the owner if
    you lack them.
- **Slow checkouts.** Full-history checkouts on Blacksmith are slow without
  `filter: blob:none`. Every full-history checkout in `ci.yml` uses it; keep
  it. Adding it to other workflows (`opensecret-eif*.yml`, `release.yml`,
  publishers) is an OpenSecret or release change.

## Nightly failures

The 04:17 `ci.yml` run and the 05:47 `nightly.yml` run report through one
`ci-nightly` issue: `nightly.yml` opens or updates it, and a green night
closes it. A missing or unfinished `ci.yml` nightly run counts as a failure,
and active quarantines keep the issue open. The 05:28 CodeQL scan and the
10:37 Agent Dev desktop build are not in the issue; their failures show only
as red checks on the master commit.

## Quarantine

`CI_QUARANTINE` is for emergencies the owner asks for; fix the lane instead
([design](design.md#quarantine), [CI](../../../../docs/ci.md#quarantine)).

## Release-adjacent signals

- A master merge that touches any path outside `ci_evidence.unrelated_path`
  (anything but `apps/maple-agent/`, `docs/`, `.agents/` and root
  `AGENTS.md`) makes older Research evidence unusable. A release's
  version-bump push re-runs the evidence workflows, so the risk is a merge
  between a bump and its release. Tell the release owner; do not work around
  `ci_evidence.py`.
- Files below a component's root, READMEs included, are build inputs. On
  master they start signed builds: an Agent one through
  `plan.py only agent-desktop`, a Research one through `change_detection.py`.
  A partial Research build (one platform only) is not usable release
  evidence either.
- `services/agent-updates/**` changes select every signed Research build on
  master. Never push to `master` to test them.
- A red CodeQL scan blocks the next release, not `ci-ok`.
