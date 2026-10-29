# Following Pi

The `pi-*` crates preserve Pi's design and behavior in ordinary Rust. Maple
product policy, storage formats, account rules and UI reconciliation belong in
`maple-agent` and the app. A generic API still needs an explicit reason when it
adds behavior absent from Pi.

The initial reference is Pi `v1.0.4`
(`7c10bd4337495ee613f2224843ecdf349b80d1df`). The initial port and its accepted
language/API differences are recorded in
[Maple #1104](https://github.com/MaplePrivacyLabs/Maple/pull/1104).
This file records the storage follow-up's adaptations; it does not claim a
complete audit of every earlier difference.

## Storage adaptations

| Interface | Reason and boundary | Regression evidence |
| --- | --- | --- |
| `SessionBase`, `SessionManager::open_partial` and storage context hooks | Resume a long conversation from the required compaction context without loading all saved history. Original entry IDs and parents remain intact. SQL, caches and paging stay in Maple. Complete-load behavior remains available; operations requiring an unloaded prefix must require a complete reload. | Full-versus-partial projection comparisons, inherited model/thinking/system state, compaction, retry and fork/export restrictions in `pi-coding-agent/src/session.rs`. |
| Explicit `flush` and `has_unpersisted` | Expose pending persistence so an embedding host can retry. Maple decides whether shutdown or eviction may finish. Consuming an error must not discard the pending entries. | Persistence failure/retry tests in `pi-coding-agent/src/session.rs` and Maple store tests. |
| Optional tool-output directory | Let an embedding host choose the lifetime and location of overflow files. Standalone defaults remain available. An invalid explicit directory fails without a fallback. Maple owns task paths, account policy and cleanup. | Shell, user-bash and private-directory tests in `pi-coding-agent`. |
| Optional shell default deadline, absolute per-call `workdir`, and quiet status updates | An embedding host needs to bound silent commands and make their execution folder and deadline visible. Pi retains no default deadline; Maple selects 30 minutes for slow builds and guides searches to request a shorter timeout. Workdir is explicit per call and never changes the session folder. Status travels through existing partial-result events, with no prompt/output logging. | Disposable-folder and blocked FIFO pipeline tests in `tools/bash/tests.rs`; saved-task cold-load and quiet-progress/Stop tests in Maple's `agent/shell_tests.rs`. |

These are deliberate embedding adaptations. They do not authorize changing the
agent loop, compaction decisions or the meaning and ordering of Pi events.
Revisit each interface when upstream offers an equivalent supported primitive;
prefer that primitive and remove the local adaptation when behavior remains
equivalent.

Maple obtains ordinary source-entry identities through its `SessionStore`
adapter. `EntryAppended` retains Pi's special-entry behavior. Live source
identity and successful durable persistence are distinct facts; a failed write
must not invent a new source identity on retry. Storage callbacks must not
re-enter Pi while its session mutex is held.

## Upstream reviews

Review useful upstream design changes and fixes manually, approximately monthly.
There is no automatic line-by-line update or requirement to adopt every release.

1. Start from the last full review below and inspect changes in the modules the
   crates follow. Keep omitted UI/provider features outside the review scope.
2. Record adopt/skip decisions and reasons. Explain any new adaptation and why
   existing hooks or adapters cannot implement it cleanly.
3. Revisit the storage adaptations above. Preserve existing event contracts;
   review behavioral changes separately from interface additions.
4. Implement selected changes in Rust with scripted-provider and boundary
   regressions, then run `just ci` in the component's pinned environment.
5. Record the reviewed Pi revision, scope, decisions and validation in a PR and
   append a short entry here.

## Review log

- 2026-10-21: recorded the storage adaptations above. Inspected only the
  `entry_appended` contract in Pi `v1.3.0`
  (`71023aec1546d6d677a65669c5665c0927ce0dd0`): ordinary message persistence
  still does not emit it. This was a focused contract check, not a full upstream
  review or a Pi update.
