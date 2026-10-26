# Agent CI performance audit

This change addresses [issue #1224](https://github.com/MaplePrivacyLabs/Maple/issues/1224).
The baseline below comes from completed Actions traces collected October 22,
2026 (Pacific time), before the cache and job changes. Runs use different
revisions and runner conditions; these are observations, not a controlled
performance benchmark.

| Agent CI run | Workflow elapsed | Windows workspace build/test | Windows provider suite |
| --- | ---: | ---: | ---: |
| [39885038207](https://github.com/MaplePrivacyLabs/Maple/actions/runs/39885038207) | 67m36s | 45m00s, cache miss | 18m16s |
| [39890827847](https://github.com/MaplePrivacyLabs/Maple/actions/runs/39890827847) | 71m02s | 47m01s, cache miss | 19m23s |
| [39876263031](https://github.com/MaplePrivacyLabs/Maple/actions/runs/39876263031) | 52m13s | 29m29s, target cache restored | 19m25s |
| [39842844293](https://github.com/MaplePrivacyLabs/Maple/actions/runs/39842844293) | 77m52s | 48m03s, cache miss | 20m11s |

Windows controlled the critical path. In the baseline its workspace build
reported 33m07s development compilation and 6m03s test compilation; the
provider reported 18m00s compilation and about 11 seconds executing tests.
Linux additionally built an optimized binary for 19m52s after its other checks.
Job creation-to-start delays were 3–9 seconds across 15 sampled platform jobs.
The evidence identifies compilation and serial scheduling costs; it does not
identify CPU, memory or disk saturation.

Local validation also found Cargo selecting a home-directory Rustup Clippy
1.97 proxy while the Nix shell selected Rust/Cargo 1.98. The component shell
now pins `RUSTUP_TOOLCHAIN` to its Nix toolchain, covering external Cargo
subcommands such as Clippy and rustfmt. Linux CI fingerprints Clippy's actual
version as well. This local observation is not a measurement of hosted Clippy
selection or the time saved by the correction.

Cache behavior was mixed: all baseline platform lookups missed, while later
runs restored some workspace targets. All twelve Desktop build lookups across
[39885037989](https://github.com/MaplePrivacyLabs/Maple/actions/runs/39885037989),
[39891392302](https://github.com/MaplePrivacyLabs/Maple/actions/runs/39891392302)
and [39876520067](https://github.com/MaplePrivacyLabs/Maple/actions/runs/39876520067)
missed. Nix installer-cache hits are separate from Cargo target-cache reuse.
The old user key put the full Cargo.lock/flake digest in both exact and restore
keys, preventing compatible fallback after a version change. The upstream
provider suite compiled into a disposable target outside that workspace cache.

In [cancelled master run 39891392236](https://github.com/MaplePrivacyLabs/Maple/actions/runs/39891392236/job/120211837972),
Windows completed 45m20s of workspace build/test, then ran the provider for
12m14s before cancellation. Its workspace cache-save post step never ran.
Independent provider jobs let completed workspace jobs save earlier without
changing master cancellation or signed Desktop concurrency policies.

The implementation preserves the workspace feature matrix, upstream provider
features and provenance checks, static linkage, source regeneration, notices,
optimized binary, and all package/signing/download validations. Compiler/native
compatibility and dependency identity are separate cache boundaries; provider
test artifacts have a dedicated exact-graph cache. Signing, publication and
credential boundaries are unchanged.

## Validation after a trusted cache seed

New cache namespaces are initially cold. PRs cannot seed them because writes
remain restricted to successful master pushes. Do not weaken this rule to
benchmark a PR. Before claiming issue acceptance:

1. Retain the first successful trusted run's three-platform workspace/provider
   and Desktop build logs and `maple-agent-*-metrics-*` artifacts. Record exact
   revision, actual compiler/native identity, cache keys/hit state, action
   restore/save bytes and durations, phase durations and target sizes.
2. Compare a subsequent run using that same dependency/toolchain configuration,
   including a version-only application bump. Confirm compatible restore keys
   and Cargo's reuse separately from total workflow wall time. A PR after a
   trusted seed can demonstrate read-only restore.
3. In unprivileged validation branches, change provider revision, resolved
   upstream test-only lock, helper, toolchain/native ABI, and flags/features one
   boundary at a time. Check that the matching compatibility or exact provider
   key changes and incompatible artifacts are not restored. Local regression
   tests cover fingerprint changes; hosted invalidation remains separate proof.
4. Report wall time, sum of job durations (runner elapsed compute), cache
   transfer/archive bytes, storage usage and failed/cancelled runs for cold and
   warm cases. Keep selector-only runs out of build comparisons. Distinguish
   assertion failures and artifact-service errors from capacity problems.

Moving the baseline Windows provider off the serial path removes an 18-minute
dependency edge; it does not establish an 18-minute end-to-end saving. Extra
jobs duplicate setup/metadata work and maintain additional target caches. Cache
restore/save overhead, storage eviction, test-only resolution changes, and the
new slowest job determine the result. Dev/Prod packages and the plain CI binary
have different build inputs; their outputs have not been proved interchangeable.
Runner upgrades and further scheduling experiments should follow these results.

## macOS cache retention and repeated application compilation

The [master macOS workspace job at `dbb8e397`](https://github.com/MaplePrivacyLabs/Maple/actions/runs/39915101523/job/120286893796)
passed in **18m31s** after restoring its exact target cache. Its measured build
phase was **8m26s** and its test phase **4m42s**. Native dependencies were reused.
The [later `c9cf8522` job](https://github.com/MaplePrivacyLabs/Maple/actions/runs/39919954389/job/120302912965)
passed in **50m08s**, with a cache miss and **33m21s** measured compilation. Its
SQLCipher/OpenSSL native build alone occupied **13m23s**, overlapping other
compilation. Source revisions and hosted runners differ; these are observed
phases, not a controlled hardware benchmark or additive estimates of savings.

The two environment identities differed only in the complete Agent `justfile`
hash. Three Linux packaging-test recipe additions had invalidated macOS and
Windows even though those workspace jobs invoke Cargo directly. Configuration
fingerprints now follow the command owner:

- Linux workspace CI retains the entire `justfile`, including recipe variables
  and dependencies. Direct macOS/Windows workspace builds do not hash it.
- Nix flake inputs apply to macOS/Linux; the Windows native-setup helper applies
  to Windows. Provider and release helpers apply to their respective profiles.
- Actual Rust/Cargo, compiler/native binaries, SDK, flags, Cargo configuration,
  manifest features/profiles and common SQLCipher provenance checks remain
  part of the compatibility boundary. Exact dependency keys and successful
  master-only cache writes retain their existing policies.

Changing this fingerprint policy intentionally creates a new cache generation.
Its first successful master run must seed it; a cold first PR is not evidence
that the new scope failed. Keep sufficient storage for overlapping native-cache
generations. The read-only audit found a configured **10 GiB** GitHub cap and
**10.085 GiB** of entries, including an obsolete **2.42 GiB** Windows generation.
That establishes quota pressure, not the cause of every missing entry. Blacksmith
Desktop/Linux caches use a separate backend and are outside that GitHub total.

The app build script also watched Git reference files when they did not exist.
Cargo treats missing watched inputs as dirty, so an unchanged build followed by
test could rerun the script and recompile the app test binary. The warm macOS
sample repeated **3m18s** of app test compilation; the later cold sample repeated
**5m22s**. Hosted timings identify the repeated units; they did not capture the
original Cargo fingerprint reason.

The production Git metadata helper now watches existing Git pointers, using the
nearest existing parent for an absent active branch reference. That parent is
needed to detect a packed branch advancing into a newly created loose ref.
Simply ignoring all missing references can embed an old commit SHA. Offline
fixtures import the production helper and verify unchanged build/test freshness
plus full/short SHA correctness across detached, loose, packed and linked
worktree transitions. The existing dirty-suffix refresh policy remains intact.
The platform CI jobs and component checks run those fixtures with their selected
toolchain. Quantify hosted savings only after the retained Cargo reports confirm
that the repeated application unit is fresh and tests still execute.

## Incidental host toolchains must not block a compatible restore

The [later master macOS job at `692b33ff`](https://github.com/MaplePrivacyLabs/Maple/actions/runs/40024153792/job/120656381786)
passed in **57m53s** after another complete target miss. Its own actual-build
environment JSON and compatibility digest were identical to the retained
`cc67565b` seed. The cache action additionally enumerated every installed
Rustup toolchain: an unused host Rust changed from 1.99.0 to 1.99.1 while the
selected Nix Rust remained 1.98.0. That changed its supplemental restore hash
from `a5aee41c` to `a861070b`. The old compatible archive still existed; this
lookup miss was not evidence that it needed more time to warm.

`agent_cache_command.py` now runs the action's real Rust/Cargo probes through
the selected component environment, but returns an empty result for its
optional installed-Rustup inventory. The actual `rustc -vV` remains in the
primary action's identity, and its normal manifest/lock dependency suffix
remains enabled. Actual compiler, flags, SDK and native compatibility continue
to be enforced by the existing fingerprint.

When neither an exact hit nor a restored Cargo target is present, a second,
read-only action invocation searches the existing ABI/job/OS/architecture
prefix without the supplemental host-toolchain hash. Archive paths and their
cache-service version remain unchanged, allowing the existing master archives
to be used during migration. Its save step was disabled; at that stage only
the primary action saved stable exact keys after successful master validation.
That migration initially checked the restored target's `CACHEDIR.TAG`. The recovery implementation below checks
actual dependency libraries/fingerprints instead, because a valid archive can
omit the tag and otherwise trigger a second download.

The cold job spent **36m00s** building and **16m42s** preparing its environment.
Its test phase rebuilt zero compilation units, so the Git metadata freshness
fix remained effective. Cache restoration, native dependency reuse and Nix
materialization time need separate hosted measurements; a shorter overall job
alone does not establish which cost was removed.

## Pull-request package profiles

Agent desktop pull requests build only the Dev profile on macOS and Linux.
The unsigned macOS packager and Linux downloaded-package verifier use the same
Dev-only selection, including fork and stacked PRs. Master pushes and manual
master dispatches retain Dev and Prod builds, signing, attestation and package
verification. The master change selector still requires all four Dev/Prod
downloaded verifiers before considering outstanding changes covered.

This removes four runnable PR jobs (Prod macOS binary, unsigned macOS package,
Linux AppImage and downloaded Linux verifier), reducing desktop jobs from eight
to four. It does not remove platform workspace/provider tests or the separate
plain Linux release build. The job-count reduction saves their runner work;
the slowest remaining job still determines PR wall time.

## October 25–26, 2026: repository-wide recovery audit

The follow-up audit covers source at
[`6d6c259d`](https://github.com/MaplePrivacyLabs/Maple/tree/6d6c259dbfc24e05fabb0d508593fecf7820fdda/.github/workflows):
**50 workflows, 125 logical jobs before matrix expansion, and 746 steps**.
The cache inventory groups **134 declared instances across 41 workflows into
31 families**, including 79 Nix installer instances. An installer archive is
not a Rust dependency cache or a populated Nix build store. Inventory counts
record configuration; they do not prove that every instance ran or restored.

The audit includes Agent, Research, SDK, Proxy, backend, mobile, web, package
consumers and publication boundaries. It separates compilation, environment
materialization, test execution, artifact transfer and cancellation. A green
selector or aggregate with intentionally skipped children is not fresh platform
build evidence. The historical measurements above remain valid observations
of their own revisions; they do not establish present cache persistence.

### Observed cold and warm work

| Actual job | Whole job | Measured build phase | Cache and compiler evidence |
| --- | ---: | ---: | --- |
| [Earlier Windows workspace](https://github.com/MaplePrivacyLabs/Maple/actions/runs/40060011728/job/120771414230) | 24m11s | 13m37s | Compatible target archive restored; native crypto reused; tests passed and a stable master seed was published. |
| [Earlier macOS workspace](https://github.com/MaplePrivacyLabs/Maple/actions/runs/40060011728/job/120771414217) | 19m08s | 10m08s | Exact primary archive restored; native crypto reused and tests passed. |
| [`ff4c1037` Windows workspace](https://github.com/MaplePrivacyLabs/Maple/actions/runs/40229042168/job/121276681205) | 49m43s | 35m09s | Both restores missed; 908/908 build units compiled. The SQLCipher/OpenSSL native unit took 13m24s; the new seed save was rejected. |
| [`ff4c1037` macOS workspace](https://github.com/MaplePrivacyLabs/Maple/actions/runs/40229042168/job/121276681265) | 48m04s | 34m49s | Both restores missed; 1293/1293 build units compiled. The native unit took 11m56s; the new seed save was rejected. |
| [`ff4c1037` Linux workspace](https://github.com/MaplePrivacyLabs/Maple/actions/runs/40229042168/job/121276681271) | 7m04s | Not separately compared here | The exact 2,784,641,540-byte archive was extracted; the log compiled 27 crates and no native crypto crates. |
| [`ff4c1037` macOS Dev producer](https://github.com/MaplePrivacyLabs/Maple/actions/runs/40229042279/job/121276739447) | 12m51s | 10m13s | Exact release archive restored; 13/1261 units compiled, with no native crypto compilation. |

These jobs used different revisions and hosted conditions. Native compilation
units overlap other work; their durations must not be added to the complete
build phase. The observations support a substantial recovery opportunity from
durable dependency reuse, not a controlled speedup percentage or a promised
end-to-end PR duration.

The regression also appears in real PRs. The
[#1291 Windows workspace](https://github.com/MaplePrivacyLabs/Maple/actions/runs/40254500033/job/121353430523)
took 44m49s and its
[macOS workspace](https://github.com/MaplePrivacyLabs/Maple/actions/runs/40254500033/job/121353430510)
took 37m54s after both target lookups missed and native crypto compiled.
That PR was based directly on `ff4c1037` and requested the same compatibility
and dependency keys as the completed master writers. Repeated misses there
were not a new compiler/native ABI change on every PR.

### Persistence failed despite successful build jobs

Completed master runs
[`e5870485`](https://github.com/MaplePrivacyLabs/Maple/actions/runs/40172748044)
and [`ff4c1037`](https://github.com/MaplePrivacyLabs/Maple/actions/runs/40229042168)
passed their macOS/Windows builds but received the cache-service warning:

> Your usage has exceeded the allowed rate limit. Your cache is now read-only.

This is an explicit refusal to reserve new cache entries. It is not an observed
HTTP 429 status, proof of a concurrent writer, or an established zero-budget
diagnosis. The underlying enforcement cause remained unresolved at this audit.
The cache action reported the failed save as a warning, so a successful job or
post-cache step did not mean that the requested seed persisted. Existing
archives in older compatibility namespaces did not satisfy the current keys.

Blacksmith persistence must be checked separately. The Linux workspace archive
restored by the completed `ff4c1037` job was absent from a subsequent provider
inventory. A Linux Prod producer also restored an exact archive in one master
run, then missed the same key in a later run and rebuilt native dependencies.
That is same-key persistence evidence, not proof of a particular eviction
cause. A provider capacity request, a displayed quota, or an inventory entry
alone is not acceptance: retain upload receipts and prove subsequent extraction
and compiler reuse on each actual backend.

Compatibility precision remains a separate concern. Direct macOS/Windows jobs
must not inherit unrelated Linux recipe changes. The audit also found whole
flake-output hashes and cross-platform dependency-feature settings entering
compatibility decisions even when the selected compiler/native build inputs
did not change. `agent_cache_scope.py` now wraps the unchanged legacy identity
and removes only dependency declarations/feature edges proved inactive on the native
target. Unknown cfg, overrides and ambiguous features remain conservative.
Target-specific regression cases must retain invalidation for real Rust,
linker/SDK/native ABI, Cargo configuration, flags, features, profile and
SQLCipher provenance changes. Narrowing a fingerprint cannot repair a rejected
write or a vanished archive.

### SDK integration spent time preparing the wrong environment

The [#1291 SDK integration job](https://github.com/MaplePrivacyLabs/Maple/actions/runs/40254500039/job/121353433324)
passed in **34m34s**. Its database/backend setup step took **27m48s**, but the
backend Cargo build reported only **3m23s**. Approximately **24m19s** elapsed
before the full development environment was ready, including source builds of
Podman, TPM tooling, GnuPG, systemd, GPGME and BWS. The development-process
flags prevented service startup; they did not remove those packages from the
Nix shell's dependency closure.

The recovery implementation adds a separate backend `#ci` environment and
selects it in the credential-free backend and SDK integration commands. It uses
the existing pinned Rust and native-library inputs while excluding unrelated
interactive/container/credential tooling. It retains Diesel, PostgreSQL server
and client tools, and the Python dependencies used by the actual checks.
PostgreSQL server tools are necessary because the OAuth tests create disposable
databases even when the SDK backend uses the workflow's PostgreSQL service.
Migrations, OAuth persistence, TypeScript integration and Rust integration stay
selected. The default developer shell, EIF derivations, signing and deployment
inputs are separate boundaries. The 24-minute observation identifies work to
remove; hosted savings from the smaller environment are not yet verified.

### Cancellation could discard outstanding validation

In the seven-day master snapshot, specifically cancelled Agent jobs consumed
approximately **977 execution minutes** after deduplicating carried-over rerun
receipts. This is consumed runtime, not a claim that all of it is avoidable.
Successful jobs or artifacts from a cancelled workflow can still have value,
and the latest selected source must still receive complete validation.

The previous workflow-level cancellation ran before immediate-push path
selection. A selected run could be interrupted by a later unrelated push whose
selector then skipped every actual Agent job. One observed sequence was
[`6d6c259d`](https://github.com/MaplePrivacyLabs/Maple/actions/runs/40263279799),
which contained a real Linux test failure while macOS/Windows were still
running, followed by the
[PCR-only master run](https://github.com/MaplePrivacyLabs/Maple/actions/runs/40265757961),
which skipped those jobs after the earlier workflow was cancelled. Its green
selector did not certify the outstanding Agent changes.

Agent CI now selects cumulative changes from its last verified component
baseline rather than treating every immediate push as a complete baseline.
A completed successful ancestor must contain actual passing jobs for all three
workspaces, all three providers and the plain Linux release before it can be a
CI baseline. Desktop packaging keeps its separate four-consumer baseline.
Failed, cancelled or selector-only runs cannot consume outstanding work. The
existing cancellation policy remains; the latest run reselects unfinished
changes. PR updates can still replace superseded previews. Tests must cover
selected-to-unrelated, selected-to-selected, failed-to-unrelated and
cancelled-to-unrelated transitions, along with the required platform lanes.
This scheduling correction is not a new release or publication authorization.

### Recovery implementation and acceptance

The recovery implementation is present in the workflow/helpers; review and
validation are still in progress. Hosted cache persistence, test completion and
speed recovery remain unverified:

- **Trusted dependency seeds:** artifact-backed recovery is the primary read
  path, with native caches retained only as read-only bootstrap. PRs remain
  read-only. The consumer proves the designated repository's push/master run,
  exact platform/profile producer job and attempt, source commit and artifact
  creation interval. That producer job and its steps must have completed
  successfully; another lane's result is not substituted for this proof, and
  the seed does not certify the rest of the workflow. Manifest compatibility,
  API ZIP digest, archive digest, bounded sizes/file counts and safe paths are
  verified before dependency-only extraction. Git sources require a reviewed
  public-repository allowlist; unknown sources and colliding output names are
  excluded. Local/path crates and shipping
  or test executables are excluded. Provider seeds require the exact verified
  test-only graph; other compatible seeds leave locked resolution/freshness to
  Cargo. Missing or rejected seeds fall back to native bootstrap or cold build.
- **Bounded refresh and visible publication:** validated master-push builders
  prepare/upload seeds after their existing checks. Preparation or required
  upload failure fails the job. Artifacts have three-day retention; a matching
  graph restored from a proven seed created within 24 hours avoids repacking,
  otherwise a selected successful master build refreshes it. There is no new
  daily release/dispatch. Restore receipts identify the source run/artifact,
  bytes and extraction time; publication and current product tests remain
  separate evidence. Transfer and retention costs still need measurement.
- **Backend CI environment:** the backend/SDK integration calls now select the
  smaller pinned shell. Prove its closure omits unused developer tooling, then
  run the same backend and both SDK integration suites on a fresh hosted worker.
  Compare environment materialization separately from Cargo and tests.
- **Cumulative selection:** Agent CI and Desktop use their respective verified
  baselines so unrelated pushes do not consume unverified changes. Preserve the
  existing cancellation/profile policies and verify the latest selected head
  and each downloaded consumer explicitly.
- **Deterministic history regression:** a pending-history persistence fix is
  under review for the observed concurrent project/task test failure. A retry
  that happens to pass is not a fix. Require a deterministic reproduction,
  focused regressions and full selected platform tests before accepting it.

Acceptance requires a successful trusted seed-producing run and a later real
master or PR consumer for every changed cache lane. Retain exact revisions,
workflow attempts, selected compiler/native identities, restored archive or
artifact metadata, bounded Cargo timing reports and successful tests. Prove
that a version-only change reuses compatible dependencies and that a real
incompatible compiler/native/feature change does not. Compare both end-to-end
critical path and total runner time across several warmed natural runs; do not
report earlier successful seeds as proof of a later restore.

All existing coverage remains part of the acceptance contract: the three
workspace platforms, Linux feature/security checks, three verified provider
lanes, signed SQLCipher source regeneration, notices, the optimized unpackaged
Linux binary, Dev/Prod master packages, PR Dev packages and actual downloaded
consumers. The unpackaged, Dev and Prod profiles have distinct identities and
trust settings; their final binaries are not interchangeable. No improvement
here requires reducing encryption/password costs, dropping tests, merging
profile trust roots, weakening signing/provenance, changing PCR approvals,
dispatching a release or publishing/deploying an application.
