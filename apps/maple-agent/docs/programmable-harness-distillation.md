# Programmable harness: distillation of the retired prototype

> Written 2026-09-09 from a read of benthecarman/maple-gpui PR #2
> (`programmable-harness`, head 653b959) before that repository was deleted.
> It reconstructs the desktop-side modules that were **not** ported into this
> monorepo (`app/src/harness/*`, `app/src/desktop/controller_runtime.rs`,
> `app/src/backend/code_mode.rs`, the Python `maple_gpui` SDK) so that a
> production design can cite the concepts without the code. Companion files:
> `programmable-harness.md` (the original normative design) and
> `programmable-harness-preview-pr.md` (the original PR description). What
> *is* ported lives in `crates/maple-harness/`; see its module docs for which
> of the tiers below each file re-expresses.

## 0. Crate topology and the one-sentence thesis

```
crates/maple-harness      GPUI-free wire nouns: ActionId/Descriptor/Call/Response,
                          policy, audit ring, semantic targets/events, registry.
crates/maple-code-mode    GPUI-free Python worker service: kernels, processes,
                          protocol framing, authority, controller transport trait.
app/src/harness/*         Desktop ingress: catalog, ActionHost, controller bridge,
                          keymap compiler, palette, which-key, provenance, vim.
app/src/desktop/*         The GPUI root that implements the host traits.
app/src/backend/code_mode.rs  App-side ownership of the worker service.
```

**Thesis:** every state change in the app is a *descriptor-validated semantic
action* that passes through exactly one `ActionHost::invoke_observed` call,
carrying an unforgeable `TrustedInvocation` that names its actor, transport,
controller-access lease and policy epoch. Everything else (keymap, palette,
which-key, Python SDK) is a *projection or transport*, never an executor.

## 1. Core nouns

```rust
pub enum InvocationPolicy { ControllerCallable, HumanOnly }
pub enum InvocationActor  { DirectUser, Model, UserCode, Internal }
pub enum InvocationTransport { Pointer, Keybinding, CommandPalette, Python, Macro, GeneratedUi, Internal }
pub enum UiControllerAccess { Off, ReadOnly, FullAccess }   // default Off
pub enum PythonCodeMode { Off, DeveloperPreview }           // default Off
pub enum ActionEffect { Observe, Navigate, MutateMaple, ExternalEffect }
pub enum Recoverability { Ephemeral, Reversible, Irreversible }
pub enum ActionStatus { Accepted, AcceptedTerminal, Completed, Failed, Cancelled }
pub enum ActionErrorCode { UnknownAction, InvalidArguments, NotApplicable, Unavailable,
                           PolicyDenied, StaleTarget, Cancelled, Failed }
```

The authorization function is a pure five-argument decision:

```rust
pub fn authorize(policy, effect, actor, transport, controller_access) -> PolicyDecision {
    let direct = actor == DirectUser
        && matches!(transport, Pointer | Keybinding | CommandPalette);
    if actor == DirectUser && !direct { Denied(InvalidDirectUserOrigin) }
    if policy == HumanOnly { return if direct { Allowed } else { Denied(HumanOnly) } }
    if direct { return Allowed }
    match controller_access {
        Off        => Denied(ControllerOff),
        ReadOnly   => match effect { Observe|Navigate => Allowed,
                                     MutateMaple|ExternalEffect => Denied(ReadOnlyMutation) },
        FullAccess => Allowed,
    }
}
```

`TrustedInvocation` bundles: invocation id, optional `TaskIdentity`,
`ProgramId`, `RunId`, `ExecutionId`, kernel generation, actor, transport,
`controller_access`, `policy_epoch`, `CancellationToken`, `ActionBudget`, and
(for DirectUser) an opaque window-issued provenance token. `check_active`
rejects cancelled or stale-epoch leases; `admit_call` additionally consumes
budget. **One SDK request got `ActionBudget::new(1)`**: a Python call could
invoke at most one semantic action.

Audit ring: capacity 512, lifecycle `record → accept → mark_cancel_requested →
finish`. `CompletedAfterCancelRequest` exists for irreversible actions that
finish after a cancel request: cancellation is a request, never fabricated
completion.

## 2. `harness/host.rs`: the single action host

```rust
pub(crate) trait ActionExecutor {
    fn executor_identity(&self) -> &'static str;   // test proof of a single impl
    fn availability(&self, &ActionDescriptor, &ActionCall) -> Availability;
    fn execute(&mut self, &ActionDescriptor, &ActionCall, &TrustedInvocation,
               &mut ActionExecutionLease) -> ActionResponse;
}
pub(crate) struct ActionHost {
    registry, audit, authority: ProgrammabilityAuthority, policy_epoch: u64,
    direct_user_issuer_id: Option<DirectUserIssuerId>,
    admission: ActionAdmissionState,          // Active | Suspended | Quitting
    operations: HashMap<InvocationId, ActionExecutionHandle>,
}
```

Bounds: `MAX_INFLIGHT_ACTIONS = 256` (under the 512 audit ring so terminal
history is always recordable), `RESERVED_LIFECYCLE_OPERATION_SLOTS = 16`
(`run.stop`, `code_mode.stop/reset/restart/…`, `account.sign_out` keep the
full 256 while ordinary actions get 240: a saturated program budget can never
block the human's ability to stop it), `MAX_RECENT_AUDIT_SNAPSHOT = 64`.

Move-only completion capability: an async executor must
`lease.claim_completion()` and return `Accepted` carrying the leased
`operation_id`; `finish_async_action(token, response)` compares nine fields
including a secret before transitioning the audit exactly once.

The ordered gate (`invoke_inner`), to be re-implemented verbatim:

1. registry lookup → `UnknownAction`;
2. admission state: `Suspended` → `dispatcher_busy`, `Quitting` → `app_quitting`;
3. `validate_arguments` → `InvalidArguments`;
4. `validate_precondition` → `StaleTarget`;
5. DirectUser issuer must equal the host's registered issuer;
6. `check_active(policy_epoch)` → `Cancelled` or `PolicyDenied`;
7. live re-check: stored `controller_access != current` → denied;
8. `authorize` → `PolicyDenied`;
9. `availability` → `Disabled{code,message}`;
10. operation-table admission limit;
11. `admit_call` consumes budget;
12. audit `Started` with `descriptor.audit.redact(arguments)`;
13. execute.

Post-execute normalisation: mismatched ids, schema-invalid results, `Accepted`
without a claimed lease, a terminal-host descriptor returning a non-terminal
status, and duplicate identities all become `Failed`. A claimed lease with a
non-`Accepted` response cancels the invocation so spawned work is revoked.

Authority is a lease: `advance_authority` bumps the epoch and **revokes every
retained non-DirectUser operation even when the new tuple is broader**.
Shutdown: `begin_quitting()` closes admission, `cancel_all_operations()`,
then `terminalize_shutdown_operations()` is the only way to force-end audit
records.

## 3. `harness/controller.rs`: the bounded Code Mode bridge

Constants: queue depth 64, drain batch 16 per UI tick, enqueue timeout 250 ms,
response timeout 30 s, event-wait margin 1 s, max event wait 60 s, max 64
waiters, response limit 512 KiB, snapshot text limit 128 KiB, semantic page
200, list page 100, cursor capacity 512 with 5 min TTL.

```rust
pub(crate) trait ControllerUiHost {
    fn controller_policy(&self) -> ControllerPolicySnapshot;
    fn validate_controller_source(&self, &ControllerProvenance) -> Result<(), ControllerError>;
    fn semantic_snapshot(&mut self, scope: &Value) -> Result<SemanticSnapshot, ControllerError>;
    fn semantic_events_after(&self, after: u64, &SemanticEventPredicate, limit) -> Result<SemanticEventPage, _>;
    fn action_catalog(&self) -> Vec<ActionDescriptor>;
    fn resolve_action_call(&self, &ActionCall) -> Result<ActionCall, Availability>;
    fn action_availability(&mut self, &ActionCall) -> Availability;
    fn invoke_action(&mut self, ActionCall, TrustedInvocation, &SdkDeliveryBarrier) -> ActionResponse;
    fn accepted_terminal_response_sent(&mut self, &ControllerCall);
    fn collection(&mut self, ControllerCollectionKind, &ControllerProvenance) -> Result<ControllerCollection, _>;
    fn cancel_task_programs(&mut self, &KernelKey) -> Result<TaskProgramCancellation, _>;
    fn precondition_for_target(&self, &SemanticTarget) -> Option<ActionPrecondition> { None }
}
```

Lifecycle `OPEN → QUITTING → CLOSED`. `SdkRequestDisposition` is a three-state
CAS (`PENDING | UI_CLAIMED | ABANDONED`): exactly one of caller-abandon and
UI-claim wins; after the UI claims, the response wins every timeout/drop race.
`SdkDeliveryBarrier` tracks the response frame reaching the worker pipe;
`app.quit` waits on it (5 s cap) before destructive shutdown.

Drain: waiters get half the batch first, then queued requests, then waiters
again; a retained wait alone does not keep the UI loop spinning. Every
request *and every waiter poll* re-runs `authorize_call` against the live
policy (`code_mode_enabled`, `access`, `policy_epoch`, source validation).

SDK method table: `ui.describe`, `ui.query`, `ui.reveal`,
`ui.current_selection`, `actions.list`, `actions.describe`,
`actions.availability`, `actions.invoke`, `events.wait`, `tasks.list`,
`projects.list`, `transcript.list`, `mcps.list`. Discovery distinguishes
`available`, `needs_arguments` (`arguments_required` reason), and `disabled`.
Cursors bind `(kind, fingerprint, offset, revision, policy_epoch, expiry)`;
a changed revision or epoch invalidates paging. Outgoing list items are
scanned for sensitive key names and every response is size-bounded.

## 4. `harness/catalog.rs`: the descriptor catalog

About 180 actions declared through `CatalogSpec` macros with `FieldKind`
(`Bool, Count, Digit, McpServer, Number, Object, String, StringAllowEmpty,
StringArray, UuidString, Choice`). Schemas always set
`additionalProperties: false` with an explicit `required` list. Durable
side-effect actions return `oneOf {operation_id} | {local_applied, durable,
error?}` so the UI can tell in-memory from on-disk. `mcp.add/update` are
secret-bearing. Every `bindable` descriptor requires a registered typed
adapter, so a missing GPUI adapter is a startup/test failure.

Human-only class: authentication, sign-out, clipboard-image attach, all raw
text-input editing and Vim grammar primitives, `timeline.copy_selected`,
`shortcuts.reset_profile`, and both Code Mode authority setters. A program can
never widen its own authority, log in, or synthesise keystrokes.

Representative sample:

| id | effect / policy / recoverability | bindable, terminal | precondition | arguments |
|---|---|---|---|---|
| `app.quit` | ExternalEffect / ControllerCallable / Irreversible | yes, **yes** | none | none |
| `settings.open_section` | Navigate / ControllerCallable / Ephemeral | yes | none | `section` |
| `shortcuts.reset_profile` | MutateMaple / **HumanOnly** / Irreversible | no | Setting | none |
| `account.sign_out` | MutateMaple / **HumanOnly** / Irreversible | no | none | none |
| `auth.password_submit` | ExternalEffect / **HumanOnly** / Irreversible | no | none | `email, password` |
| `task.open` | Navigate / ControllerCallable / Ephemeral | yes | Task | `task_id` |
| `task.set_archived` | MutateMaple / ControllerCallable / Reversible | no | Task | `task_id, archived` |
| `project.set_trusted` | MutateMaple / ControllerCallable / Reversible | no | Project | `canonical_root, trusted` |
| `composer.set_text` | MutateMaple / ControllerCallable / Reversible | no | Draft | `task_id, text` |
| `composer.send` | ExternalEffect / ControllerCallable / Irreversible | yes | Draft | `task_id?` |
| `text_input.paste` | MutateMaple / **HumanOnly** / Reversible | yes | none | none |
| `composer.vim.motion` | Navigate / **HumanOnly** / Ephemeral | yes | none | `motion; count?` |
| `run.stop` | MutateMaple / ControllerCallable / Irreversible | yes | Task | `task_id` |
| `permission.respond` | MutateMaple / ControllerCallable / Irreversible | no | Permission | `request_id, decision` |
| `ui.activate_selected` | MutateMaple / ControllerCallable / Ephemeral | yes | none | none (contextual alias) |
| `code_mode.execute` | ExternalEffect / ControllerCallable / Irreversible | no | Task | `task_id, code` |
| `code_mode.set_controller_access` | MutateMaple / **HumanOnly** / Reversible | no | Setting | `access` |

## 5. `harness/provenance.rs`: direct-user provenance

GPUI actions carry no origin, so the window mints an opaque, non-serialisable
token only from observed physical input and consumes it exactly once.
`WindowProvenance` tracks focus/context/keymap generations plus separate key
and pointer dispatch phases. Pointer: the root opens a phase in capture; one
child may consume once while the event bubbles; mouse-up opens a fresh phase.
Key: `begin_key_sequence()` snapshots the three generations and
`consume_resolved_key()` succeeds only if all still match. A pending chord
closes the immediate dispatch window while retaining provenance, which stops a
programmatic `dispatch_action` from stealing the token.

## 6. `harness/keymap.rs`: the pure keymap compiler

Limits: 1 MiB file, 8 strokes per sequence, 4096-byte contexts. JSON format:
sections `{"context": "<predicate>", "bindings": {"<seq>": <directive>}}` with
directives `"action.id"`, `["action.id", {...}]`, or `null` (an explicit
unbind). A parallel token scanner attaches line/column to every entry.

`ContextAnalysis` decomposes top-level `&&` conjunctions into facts
(`Present, Absent, Equals, NotEquals`); any `||` makes overlap `Possible`.
Precedence: user layer over template, later over earlier, specificity
informational. Pipeline: validate everything (any diagnostic aborts the whole
compile), assign ids, attach defaults, resolve exact groups (a winning `null`
disables the whole group), classify conflicts (`Possible > Prefix > Shadow >
Exact`), build the prefix trie from install rules. Which-key continuations
pick the terminal maximising `(context depth, precedence)`, matching GPUI's
own resolution order. Install is all-or-nothing with a last-known-good
fallback; files are written atomically (same-directory 0600 temp, fsync,
rename, fsync parent) under an FNV-1a + byte-length revision CAS.

Vim template context design: `MapleApp && profile == vim && app_vim_mode ==
normal && !TextInput && !ApplicationModal`. Negating the leaf component is the
actual shadow, so ordinary text/IME insertion stays with the focused editor.

## 7. `keymap_runtime.rs`, `shortcut_store.rs`

The runtime adapter validates by constructing the typed GPUI action, so a
successful compile cannot fail during installation. The shortcut store runs
off the UI thread, requires an expected file revision (never `Any`), parses
and rejects a malformed current file first, recompiles the whole prospective
keymap before writing, and turns "replace conflicting bindings" into explicit
`null` overrides.

## 8. `palette.rs`, `which_key.rs`, `application_vim.rs`

Palette ranking ladder: exact id/alias 1000, exact label 950, id/alias prefix
900, label prefix 850, exact shortcut 825, substring 500, else filtered. A
leading `:` is stripped. Missing required arguments become an inline JSON
editor seeded from the schema, not a disabled row. Navigation inside the
palette is itself semantic (`ui.select_next`, `ui.activate_selected`).

Which-key: 300 ms delay, 5 s manual timeout, rows grouped by category with
`"{category} commands"` labels for pure prefixes, a generation-fenced
`Hidden/Waiting/Visible` machine, and portable keystroke tokens rebuilt from
modifier fields so the projected prefix matches the compiled trie.

Application Vim count prefix: digits arrive as the typed
`application.vim.count_digit{digit}` action, never as raw keys; an explicit
allowlist of 18 countable actions; leading zero is a no-op; saturates at
999 999; a pending prefix beats an explicit `count` argument.

## 9. `harness/semantic.rs`, `desktop/controller_runtime.rs`, `backend/code_mode.rs`

Semantic projection keeps `entries` (navigable) separate from `known` (all
candidates) so a filter is never reported as a deletion. Selection
reconciliation is by stable target, then filtered retention, then a
successor-preferring neighbour search, then region fallback. `RegionGraph`
gives `ctrl-w hjkl` over a semantic graph rather than pixels.

The GPUI runtime: final-action reauthorisation maps contextual aliases to one
concrete call on both discovery and execution paths; the real host is swapped
for `ActionHost::suspended()` during dispatch so nested dispatch fails with
`dispatcher_busy`; a root/worker tuple or epoch mismatch reports the
controller as `Off` (split-brain fails closed); source validation ties
provenance to the signed-in account and a live task.

`AppCodeMode` serialises every authority/account transition under one mutex,
requires transitions to advance exactly one epoch or be an exact retry, and
revokes a program with two independent attempts (worker lease and host-bound
model run). History is bounded (64 per task, 64 tasks, 8 MiB).

## 10. `maple_gpui` Python SDK

Always importable; Rust is the authority boundary. `SemanticObject(dict)` and
`SemanticPage(tuple)` (a dict would shadow `page.items`). The flagship pattern
is `actions.invoke_and_wait`: capture `event_cursor` **before** invoking, then
`events.wait({kind: action_completed, invocation_id}, after=cursor)`, so a
completion published before the wait registers is still found in the ring.
No polling, no sleep.

## 11. Invariant checklist

1. One execution path; `executor_identity()` proves it in tests.
2. Descriptor first: arguments, results and preconditions validated in and out.
3. Human-only actions need a real pointer/key/palette origin.
4. Provenance is a move-only, window-scoped, generation-fenced capability.
5. Authority is a lease; any epoch change cancels retained non-human work.
6. Stored `controller_access` must equal the live tuple at execution time.
7. Split-brain fails closed.
8. One atomic pre-dispatch winner per SDK request.
9. Final-action reauthorisation on the concrete action.
10. Exactly-once terminal completion via a secret-bearing token.
11. Cancellation is a request; `CompletedAfterCancelRequest` records the truth.
12. Everything is bounded (256 ops, 512 audit, 64 queue, 16/tick, 64 waiters, 512 cursors, 512 KiB responses).
13. Revisions everywhere (cursors, keymap files, semantic targets).
14. All-or-nothing keymap install with last-known-good.
15. Redaction by construction.

## 12. Minimal faithful re-implementation, in priority order

**Tier 1, the gate:** `ActionId`/`ActionCall`/`ActionResponse`;
`ActionDescriptor` + registry with adapter enforcement; the policy enums and
pure `authorize`; `ProgrammabilityAuthority` + epoch snapshot; `ActionBudget`
+ `TrustedInvocation`; the audit ring; the `ActionHost` gate with async
completion tokens and epoch-based revocation. *(Ported: action, registry,
policy, audit; re-expressed: `host.rs`.)*

**Tier 2, catalog and ingress:** the declarative catalog with the Human Only
set intact; `DirectUserIngress` and a headless provenance equivalent.
*(Re-expressed: `catalog.rs`; ported: `DirectUserIngress`/`PendingKeyProvenance`.)*

**Tier 3, the programmable bridge:** `SdkRequest`/`ControllerProvenance`,
disposition and delivery fences, the bounded bridge with waiter fairness, the
13-method dispatcher with cursors and redaction, event waits over the semantic
ring. *(Re-expressed in compact form: `controller.rs`; ported: semantic events.)*

**Tier 4, keymap and surfaces:** sequence/context analysis, `compile_keymap`
with precedence/conflicts/trie, all-or-nothing install with last-known-good,
atomic file CAS, the palette model, which-key projection, Vim count state.
*(Ported: `keymap.rs` core; re-expressed: `discovery.rs`.)*

**Tier 5, semantics:** targets, presence, projection, reveal plans, selection
reconciliation, region graph, return stack, revision ledger. *(Ported:
`semantic.rs`.)*

**Tier 6, optional Python worker:** kernel identities, worker service traits,
the SDK facade shapes and the cursor-before-invoke protocol. *(Not ported;
the bundled CPython runtime landed separately as Maple #897.)*
