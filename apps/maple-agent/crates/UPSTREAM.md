# Following Pi

The `pi-*` crates preserve Pi's design and behavior in ordinary Rust. Maple
product policy, storage formats, account rules and UI reconciliation belong in
`maple-agent` and the app. A generic API still needs an explicit reason when it
adds behavior absent from Pi.

The crates follow Pi `v1.4.0` (`4a080252f06caf63a525d7873c5d94d3c23887e0`,
2026-10-22), the end of the latest full review below; start the next review
there. The initial port from Pi `v1.0.4` and its accepted language/API
differences are recorded in
[Maple #1104](https://github.com/MaplePrivacyLabs/Maple/pull/1104); each
review's PR records its adopt/skip decisions and any new differences.
This file records the storage follow-up's adaptations; it does not claim a
complete audit of every earlier difference.

## Storage adaptations

| Interface | Reason and boundary | Regression evidence |
| --- | --- | --- |
| `SessionBase`, `SessionManager::open_partial` and storage context hooks | Resume a long conversation from the required compaction context without loading all saved history. Original entry IDs and parents remain intact. SQL, caches and paging stay in Maple. Complete-load behavior remains available; operations requiring an unloaded prefix must require a complete reload. | Full-versus-partial projection comparisons, inherited model/thinking/system state, compaction, retry and fork/export restrictions in `pi-coding-agent/src/session.rs`. |
| Explicit `flush` and `has_unpersisted` | Expose pending persistence so an embedding host can retry. Maple decides whether shutdown or eviction may finish. Consuming an error must not discard the pending entries. | Persistence failure/retry tests in `pi-coding-agent/src/session.rs` and Maple store tests. |
| Optional tool-output directory | Let an embedding host choose the lifetime and location of overflow files. Standalone defaults remain available. An invalid explicit directory fails without a fallback. Maple owns task paths, account policy and cleanup. | Shell, user-bash and private-directory tests in `pi-coding-agent`. |

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
- 2026-10-30: full review of Pi `v1.0.4`..`v1.4.0` (`packages/ai/src`,
  `packages/agent/src`, `packages/coding-agent/src/core` and `src/utils`) in
  [Maple #1389](https://github.com/MaplePrivacyLabs/Maple/pull/1389):
  22 upstream commits adopted, among them dropping another model's thinking,
  thinking-free summaries without a fixed output cap, and `before_agent_start`
  for custom-message runs; provider, catalog, terminal and unported features
  skipped. Upstream's session manager and tool output did not change, so the
  storage adaptations stay. Skipped `bd3c9b823` (activating extension tools
  registered after a branch recorded its tools): it needs every tool the
  branch ever named, which a `SessionBase` does not carry. Validated with
  `just ci`.
