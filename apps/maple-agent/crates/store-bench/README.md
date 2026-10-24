# Store regression measurements

`store-bench` measures the actual new Store implementation with synthetic data.
It does not discover previous Maple data, make model requests, or connect to a
service. Ordinary benchmark runs use temporary profiles and hold the host's OS
ownership lock. Product binaries do not enable the `store-bench` feature.

Run from `apps/maple-agent` through its pinned Nix environment:

```sh
nix develop --no-update-lock-file . -c just store-bench-ci
nix develop --no-update-lock-file . -c just store-bench
```

The CI fixture has 40 tasks / 8,000 entries, including a 2,000-entry hot task.
The full fixture has 2,000 tasks / 1,000,000 entries, including a 100,000-entry
hot task and a 20,000-entry long task. The deterministic generator adapts the
research harness's shape to current typed Pi messages. It creates real user
messages, assistant text/tool calls, tool results, todo lists, thinking changes,
and compaction boundaries. Source and derived timeline rows are inserted in
global timestamp order across tasks.

Fixture initialization batches at most 1,000 source entries in each Durable
transaction, outside timed scenarios. Its feature-gated adapter reuses the
production entry preparation, cache derivation and timeline projector. Adapter
states become visible only after COMMIT. Timed source appends call the ordinary
production SessionStore path one entry at a time. Every report verifies the
effective writer settings are `synchronous=FULL`, `fullfsync=ON`, and
`wal_autocheckpoint=0`; it records every connection's relevant pragmas.

The report includes nearest-rank median, p99, p99.9, maximum and count over
5 ms for source appends; warm and fresh-connection list/open/history reads;
full and compaction-boundary context loads; raw-key connection open; held-reader
WAL/checkpoint behavior; rebuild preparation and writer chunks; attachment
chunks; a 20 MiB oversized write and range reads; deletion including its
oversized source-row exception, blob chunks and final metadata; synthetic JSONL
comparisons; and Argon2. Full plain runs additionally measure conversion with
the floor password KDF included explicitly in the reported duration.

"Cold" means new SQLite connection caches while the OS cache remains warm.
These measurements do not establish cold-disk, UI, live-overlay, or network
latency. The JSONL append comparison intentionally uses historical unsynced
appends and is labeled accordingly. It does not satisfy the Store's durability
contract. The oversized-row and blob-deletion paths report their actual chunk
sizes rather than treating exceptions as a 1 MiB guarantee.

Each rebuild and collection chunk records its row/byte count, elapsed writer
call and WAL file size before/after. The signed WAL delta measures observed
file growth or reuse; it is not physical WAL bytes written. Writer-call time
includes admission and commit, so it bounds the writer hold rather than
isolating mutex hold time. Process peak RSS is a whole-process high-water mark
including fixture generation, binary hashing and both modes.

Each JSON result has a sibling SHA-256 file and records engine/provider
versions, source revision/dirty state, executable digest, OS/CPU/memory, load,
dataset identity and settings. No synthetic database or key is uploaded.

## References and CI

`scripts/evaluate.py` requires both modes, the canonical seed/scale, all
scenarios and their sample counts, effective durability settings, and indexed
plans with and without statistics. Missing scenarios cannot silently remove a
slow operation. Linux CI compares medians at 10x its actual Linux reference;
the scheduled `xcode-27` full run uses 2x its actual macOS reference. Platform
mismatches are rejected. Native Mac small runs can use `--profile linux-ci
--diagnostic` to validate workload evidence without comparing or recording a
Linux runner gate.

For a native Mac small result, run from `apps/maple-agent`:

```sh
python3 crates/store-bench/scripts/evaluate.py target/store-bench-ci.json \
  --profile linux-ci --diagnostic
```

The full macOS evaluation also checks the documented absolute limits:

| Measured scope | Limit |
| --- | --- |
| Keyed fresh-connection list request, 50 rows | median ≤ 1.25 ms on full macOS; 1 ms for Linux diagnostics |
| Keyed fresh-connection open request, 100 rows and plan lookup | median ≤ 2 ms; Store only, UI/live overlay unmeasured |
| Keyed fresh-connection history request, 200 rows at measured depth ≥ 40k | median ≤ 3 ms; full dataset only |
| One raw-key SQLite connection | median ≤ 1 ms; host startup/password KDF excluded |
| First full 20k-entry context load including parse | median ≤ 250 ms keyed / 150 ms plain; full dataset only |
| First keyed context load from compaction | median ≤ 50 ms |
| Keyed first 10 MiB attachment range assembly | ≤ 25 ms; SQLite-reader caches fresh, OS cache warm |
| Capped rebuild/collection calls, both modes | maximum of actual chunks ≤ 50 ms |

The full macOS list budget includes 25% hosted-runner headroom after the
[scheduled run](https://github.com/MaplePrivacyLabs/Maple/actions/runs/39986418131/job/120526655680)
measured 1.019 ms against the previous 1 ms limit. This is an explicit tolerance
choice, not an estimate of runner variance from repeated measurements. It changes
only this absolute list budget; relative median comparisons, query-plan guards,
fixture validation and other absolute limits are unchanged.

Oversized single-row chunks are reported separately against the 50 ms target.
A miss produces `needs_decision`, blocking full acceptance until chunk storage
or a measured alternative is reviewed. Neither a fast median nor a runner
factor excuses a slow chunk. The Linux small fixture reports applicable
absolute diagnostics but does not enforce the full macOS budgets.

`reference.json` initially contains explicitly pending references. An absolute
pass does not establish a relative baseline; missing or partial references
remain `baseline_pending`. `--require-reference` fails on that state. No old
NORMAL-mode append target is enforced: actual FULL/platform-sync medians and
tails must establish reviewed append budgets. The harness does not measure UI,
live overlays, network, cold disk, physical per-chunk WAL writes, or isolated
operation RSS; it also does not emit the long task's individual payload bytes
to confirm the historical 37 MB shape. The report states these limits.
Small runs use the Argon2 floor; full runs calibrate on the current host when
RAM is available. This does not prove low-end cross-machine cost caps. Missing
RAM information, including the current Windows helper, falls back to the floor.

Do not insert estimated timings from the research VM or weaken durability to
make a reference pass. Record a reference only from an actual completed runner
artifact and include before/after run links in the PR changing this file:

```sh
python3 crates/store-bench/scripts/evaluate.py downloaded-result.json \
  --profile macos-full --record-reference \
  --run-url https://github.com/MaplePrivacyLabs/maple/actions/runs/ACTUAL_RUN
```

Recording refuses unresolved full-run absolute limits. A reference compares
medians only; append tails, checkpoints and conversion throughput remain
reported evidence without an invented acceptance threshold. The report's OS
does not independently attest the scheduled runner label or hardware.

The separate performance workflow runs small Linux data on relevant PRs and
pushes, and both Linux and full macOS data on its schedule or manual dispatch.
Workflow artifacts are results and digests, with no signing or release action.

## Seed a stopped Local profile

The explicit `seed` command uses the selected MDE's absolute `XDG_DATA_HOME`,
defaults to the unpackaged `maple-agent` namespace, requires a signed-in account
already present in its new plain store, and refuses a running owner. It never
uses a saved password or creates an account. `--namespace` may explicitly select
`maple-agent-dev` or `maple-agent-prod`; it never guesses another profile.

```sh
# Source the selected MDE's env/maple-agent.sh first and stop that exact host.
nix develop --no-update-lock-file . -c just store-bench seed \
  --tasks 200 --entries 100000 --hot-task 20000 --long-task 5000 \
  --project /absolute/path/to/the/selected/project
```

The command prints the database path/size and fixture counts. Reusing synthetic
task IDs is refused rather than overwriting existing new-store tasks.
