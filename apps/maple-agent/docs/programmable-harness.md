# Maple Programmable Harness

> **Preserved design, 2026-09-09.** This document is carried over unchanged
> (apart from this preface) from the `maple-gpui` Developer Preview branch,
> benthecarman/maple-gpui PR #2 (`programmable-harness`, head 653b959), before
> that repository is deleted. The full prototype (about 90 files, including the
> GPUI wiring, composer Vim engine, shortcut settings, and the `maple_gpui`
> Python SDK) is **not** ported. What this monorepo keeps is:
>
> - `crates/maple-harness/` — the GPUI-free contract crate (actions,
>   registry, policy, semantic snapshot/events, audit, keymap) ported as-is;
> - `crates/maple-harness/src/{host,catalog,controller,discovery}.rs` — a
>   compact from-scratch re-expression of the desktop-side concepts (single
>   action host with final-action reauthorization, descriptor catalog,
>   bounded controller bridge, palette and which-key) written so that the
>   ideas compile and are unit-tested without a window;
> - `docs/programmable-harness-distillation.md` — a module-by-module
>   reconstruction of the desktop code that was not ported, with the
>   invariant checklist and a tiered re-implementation order;
> - `docs/programmable-harness-preview-pr.md` — the original PR description.
>
> Sections below that describe GPUI screens, `app/src/harness/*`, or the
> Python worker refer to the old prototype layout and are historical. Paths
> such as `crates/maple-code-mode/` now mean Maple `apps/maple-agent/crates/`;
> the bundled CPython runtime that this design assumed landed separately as
> the cpython + codemode work (Maple #897), which this branch is stacked on.
> Nothing here is wired into the running Agent; a production version will be
> a fresh design that can cite this one.

Status: normative implementation specification for an architecture-complete Developer Preview.

Audience: reviewers, contributors, and future maintainers.

Target: macOS first. Keep the core portable and keep Linux compiling; Linux desktop behavior can be validated by a Linux reviewer.

This document is intentionally detailed. It records the product thesis, the decisions already made, the required architecture, the initial user experience, the implementation seams in the current repository, and the validation and review contract. Future changes should not replace these decisions with a smaller unrelated feature, a second command system, key simulation, an embedded arbitrary-code path, or claims of sandboxing that are not true.

## 1. Executive summary

Vim mode is not the underlying feature. It is one client of a programmable application.

Maple should expose every meaningful application operation through one typed, discoverable semantic action system. Buttons, keyboard shortcuts, a command palette, whole-application Vim, the model, and Python programs all use that same system. The model can then navigate or operate Maple by writing short programs against semantic state instead of pretending to press keys or clicking coordinates.

The architectural center is:

~~~text
pointer / keyboard / palette / Vim / model / Python / future generated UI
                                  |
                                  v
                       typed semantic actions
                                  |
                                  v
                  availability + authority + audit
                                  |
                                  v
                       one action implementation
                                  |
                                  v
                    Maple state and backend effects
                                  |
                                  v
                semantic events + native GPUI rendering
~~~

This unlocks several related experiences without building separate automation stacks:

- A Standard shortcut profile for ordinary users.
- A Vim profile with application-level modal navigation.
- Real Vim editing in the chat composer.
- A searchable command palette and shortcut editor.
- Model-controlled UI navigation and action chaining.
- A persistent per-task Python/IPython Code Mode.
- A loadable maple_gpui Python controller with Read Only and Full Access policies.
- Later, declarative generated companion views and mini-apps.

The current branch must implement the real vertical spine, not merely types or mock UI. It is acceptable for long-tail compatibility and hardened process isolation to remain explicit follow-up work. It is not acceptable to leave the central action path, policy checks, cancellation, stable semantic targeting, core Vim commands, Python lifecycle, controller SDK, or live validation as TODOs.

## 2. Document and review contract

This document began as the implementation brief for the Developer Preview and
now serves as its design and implementation record. The implementation is
intentionally end to end so reviewers can evaluate the real interaction among
actions, policy, semantic state, Vim, Python, and model control rather than a
set of disconnected abstractions.

The review contract is:

1. Treat the product thesis, dependency direction, one-path invariant,
   authority model, honest Python boundary, and fail-closed lifecycle as the
   architectural center of the proposal.
2. Keep semantic behavior on one typed action path. Do not accept a
   metadata-only registry while buttons, shortcuts, or model calls bypass it.
3. Preserve stable wire identities and semantic targets across Rust/GPUI
   refactors; do not expose GPUI entities, callbacks, or Rust type names as the
   public automation protocol.
4. Require direct-human provenance for Human Only actions and authority
   changes. A controller request cannot claim its own actor, transport,
   account, task, policy epoch, or capability lease.
5. Treat native Python as a separate process and a useful Developer Preview,
   not as a hardened sandbox. Production containment remains an explicit
   follow-up.
6. Keep every change buildable and covered at the owning layer. Run the full
   repository validation before proposing promotion beyond Developer Preview.
7. Review the implementation in the architectural slices in Section 26 even
   when integration and follow-up fixes require additional commits.

## 3. Quality bar

This is an ambitious experiment, not a throwaway prototype.

The desired balance is:

- Complete architecture and genuine end-to-end behavior.
- Good Rust boundaries and testable pure components.
- Deliberate, understandable UX.
- Honest security language.
- Clean commits and useful documentation.
- TODOs for production hardening and exhaustive edge cases.

The following are acceptable follow-ups:

- Exhaustive Vim compatibility.
- A hardened macOS helper or XPC sandbox and equivalent Linux/Windows isolation.
- Rich Python package and environment management.
- Durable Python variables across app restarts.
- Generated UI cards and companion surfaces.
- Dynamic third-party action registration.
- Exhaustive localization and accessibility polish.

The following are not acceptable shortcuts:

- Separate button, shortcut, model, and Python implementations of the same operation.
- Key or coordinate simulation as the model control API.
- An action registry that is metadata only while buttons keep bypassing it.
- Action targets based on virtual row indices or GPUI entities.
- A Read Only controller that can mutate through a generic activate command.
- A Python module that directly receives GPUI objects, backend handles, credentials, or Rust pointers.
- Python execution in the GPUI process.
- Unbounded Python output or an infinite loop without a working Stop path.
- Calling native CPython a sandbox merely because cwd and HOME were changed.
- Applying Vim interception to password, login, search, rename, or settings inputs.
- Implementing dot repeat by replaying raw keystrokes.
- Stopping after planning, scaffolding, or unit tests without launching and exercising the app.

## 4. Product thesis and inspirations

### 4.1 Vim as an application grammar

Vim contributes more than familiar movement keys. Its important ideas are:

- explicit modes;
- composable operators and motions;
- counts;
- text objects;
- repeatable semantic changes;
- command discovery through prefixes and a command line;
- a stable distinction between navigation and insertion.

Maple applies those ideas at two levels:

1. Application Vim navigates semantic Maple objects: tasks, projects, transcript items, settings rows, menus, questions, permissions, tool cards, and future annotations.
2. Composer Vim edits text with a true modal state machine.

The two levels coordinate through shared profiles and context state, but they must not be one giant state machine. A transcript j moves between timeline objects. A composer j moves between logical text lines.

### 4.2 GPUI and Zed

The current app already uses GPUI 0.2.2 typed actions, key contexts, bubbling, focus handles, and virtualized lists. GPUI also supports:

- typed parameterized actions and JSON schemas;
- dynamic action construction;
- multi-stroke bindings;
- boolean key-context predicates;
- clearing and rebuilding the complete keymap;
- enumerating current actions and bindings;
- observing pending multi-stroke input.

The intended direction follows Zed's useful patterns:

- Almost all functionality is exposed as actions.
- User keymaps bind action IDs, action arguments, sequences, and context predicates.
- Later, more specific or user bindings override templates.
- A command palette discovers and dispatches available actions.
- Vim state participates in key contexts rather than intercepting every key globally.
- Vim extends beyond the editor into panels and application surfaces.

Useful primary references:

- GPUI key dispatch: https://github.com/zed-industries/zed/blob/main/crates/gpui/docs/key_dispatch.md
- Zed key bindings: https://zed.dev/docs/key-bindings
- Zed Vim behavior and contexts: https://zed.dev/docs/vim
- Zed Vim keymap: https://github.com/zed-industries/zed/blob/main/assets/keymaps/vim.json
- Zed Vim engine: https://github.com/zed-industries/zed/blob/main/crates/vim/src/vim.rs
- Zed command palette: https://github.com/zed-industries/zed/blob/main/crates/command_palette/src/command_palette.rs

Maple should borrow the architectural lessons, not copy Zed wholesale. Maple's objects are conversations, tasks, tools, permissions, and projects rather than buffers and syntax trees.

### 4.3 Codex-style model viewpoint control

The motivating experience is a model that can cross the application boundary in a controlled, inspectable way.

For example, while the user is typing, an assistant response streams into the transcript. Streaming does not steal focus. In Vim:

1. Escape leaves Insert for composer Normal.
2. ga selects the newest assistant response, or Ctrl-W k moves to the transcript.
3. left-bracket d and right-bracket d move among annotations when annotation objects exist.
4. Enter activates or opens the selected semantic object.
5. gi returns to the composer at the stored insertion point when the draft
   revision still matches, or safely to the end of a replaced draft.

The model can perform the same navigation semantically:

~~~python
import maple_gpui as maple

snapshot = await maple.ui.describe()
tasks = await maple.tasks.list(title_contains="Nix flake")
await maple.workspace.open_task(tasks[0].id)
await maple.transcript.focus_next(kind="annotation")
await maple.events.wait(
    {"kind": "semantic_selection_changed"},
    after=snapshot.event_cursor,
    timeout=10,
)
~~~

The model should invoke the inner action directly when changing the visible viewpoint adds no user value. It should navigate visibly when the user needs to see the object or follow the flow. Both paths use the same registry and policy.

### 4.4 Python and future RLM work

Python is not only an implementation detail for UI navigation. A persistent
per-task execution environment is useful for general computation and future
recursive-language-model work. In this Developer Preview it is exercised by
model `python_code` calls and inspected from Settings; there is no human-facing
chat REPL. Therefore:

- General Python Code Mode works when UI control is Off.
- The maple_gpui SDK is a required deliverable in this branch, layered on top of general Python and importable for useful calls only when UI Controller access is Read Only or Full Access.
- The Python process is persistent per Maple task.
- IPython is preferred when available; CPython fallback still supports persistent state and top-level await.
- Code Mode is off by default and clearly labeled Developer Preview.

### 4.5 Future generated UI

Generated UI is deliberately not part of this branch, but the action architecture should make it possible later.

The safe direction is a versioned declarative view document rendered from trusted GPUI components:

- stacks and grids;
- text and Markdown;
- forms and buttons;
- tables and charts;
- images, code, and diffs;
- tabs, lists, progress, and annotations.

Events from generated surfaces would request registered actions with schema-validated arguments. Generated surfaces would never compile arbitrary model-generated Rust into Maple's authenticated process. Trusted Maple chrome would identify generated content and generated views could never impersonate permission, credential, or account dialogs.

Capability-limited WASM or a separate helper process could be considered later. Zed's extension capability model is useful evidence that this boundary deserves care: https://zed.dev/docs/extensions/capabilities

Generated mini-apps are a consumer of the programmable harness, not a reason to distort the first implementation.

## 5. Decisions already made

These are settled unless current source makes one literally impossible:

1. Application Vim is the priority. Composer Vim must still be complete enough to feel like Vim.
2. The first composer milestone includes dot repeat.
3. Standard and Vim are the only first-party shortcut profiles initially.
4. Selecting Vim replaces the Standard template. It is not layered on top of Standard.
5. User overrides are applied over the selected template and may replace or disable any shortcut.
6. The shortcut settings page is generated from every typed semantic action.
7. Every currently meaningful semantic button/control must migrate through the action path.
8. Pure pointer mechanics may remain local.
9. Kernel identity is per task, not global.
10. A task's Python environment may work with files in that task's opened project root and its private scratch directory.
11. Code Mode exposes no supported Maple network API and applies best-effort guardrails, while the native Developer Preview explicitly admits that the macOS-user Python process may still access the network and is not a hardened OS sandbox.
12. Python Code Mode and the Maple UI Controller are separate settings.
13. The controller modes are Off, Read Only, and Full Access.
14. Read Only permits observation and ephemeral navigation.
15. Full Access permits ordinary model-callable mutations without individual prompts.
16. Human Only actions can never be invoked by a model, Python, macros, or future generated surfaces, even when Full Access is selected.
17. Sign out and permanent account/identity/credential operations are Human Only.
18. Controller-authority and Code Mode enablement changes are direct-human-only so a controller cannot elevate itself.
19. Agent tool permission modes remain conceptually separate from UI controller modes. Per the agreed Full Access rule, permission.respond is nevertheless controller-callable in Full Access and must be conspicuously audited; Read Only cannot respond. Do not silently add a self-approval exception that was not agreed.
20. The implementation is a Developer Preview, but the architecture must be real.
21. The implementation is organized around the six review slices in Section 26; integration and follow-up fixes may be separate commits when that preserves history and buildability.
22. The initial implementation requires automated checks, a release-mode build,
    and live desktop validation. Later rebases and documentation-only review
    updates are revalidated in proportion to their behavioral risk.
23. Upstream review is for architectural feedback and hands-on evaluation; it does not imply that the preview is production-ready or should be merged unchanged.

## 6. Scope

### 6.1 Required in this branch

- Typed semantic action descriptors and registry.
- Dynamic action availability and disabled reasons.
- Effect, recoverability, and invocation-policy metadata.
- Host-assigned actor/transport provenance.
- Schema validation for arguments and structured results.
- Stable semantic targets and optional action-specific preconditions.
- Central policy enforcement, basic audit, and cancellation.
- Migration of every current semantic control.
- Standard and Vim templates.
- Zed-style user keymap JSON, arbitrary overrides, and null unbinding.
- Context-aware multi-key bindings.
- Conflict detection and a shortcut recorder.
- Searchable Shortcuts settings generated from the registry.
- Per-command and whole-profile reset.
- Unbound-command visibility.
- A root command palette.
- A passive which-key overlay.
- Stable semantic selection for the transcript, sidebar, settings, and modal surfaces.
- Whole-app Vim navigation with the agreed command set.
- Composer Normal, Insert, and characterwise Visual modes.
- Required composer motions, operators, counts, text objects, paste, undo/redo, and dot repeat.
- A visible composer mode indicator and mode-aware cursor.
- Persistent per-task Python worker outside the GPUI process.
- IPython when available and a useful CPython fallback.
- Output, source, protocol, runtime, and active-kernel bounds.
- Stop, Reset, Restart, crash recovery, and process-tree cleanup.
- A Settings-only Code Mode configuration, status, lifecycle, retained-program,
  and audit surface; no chat REPL or global HUD.
- Model-facing python_code tool.
- Required maple_gpui SDK, enabled for controller calls only in Read Only or Full Access.
- Semantic state discovery, action discovery/invocation, and event waiting.
- A trusted built-in maple-ui-controller skill.
- Settings, disclosure, audit visibility, and policy revocation.
- Focused unit/integration tests, complete repository CI, macOS release build, and live GUI validation.

### 6.2 Explicitly deferred

- Full Vim, Neovim, or plugin compatibility.
- Visual Line and Visual Block, named registers, marks, macros, substitutions, search grammar, and full Ex.
- Named multi-action user macros and transactional action programs.
- A hardened OS sandbox and production containment claims.
- Bundled Python distribution in every release artifact.
- Virtualenv/package-management UX.
- Durable Python namespace across app restarts.
- Broad MIME-rich Python display support.
- Generated cards, panels, or full-screen views.
- Dynamic third-party action registration and schema migration.
- Multi-window semantic navigation.
- Linux GUI validation beyond compilation/tests in this macOS-first branch.
- Retry-addressable process-cleanup ownership after a bounded worker teardown
  reports diagnostics.
- Concurrent, bounded teardown-receipt capture with durable ownership across
  waiter cancellation.
- Operating-system-backed physical-input provenance below GPUI's synthetic
  input seam.

### 6.3 Annotation boundary

Codex-style annotations are an inspiration and a semantic target type. The current Maple GPUI source does not have an equivalent first-class transcript annotation transport. This branch must:

- define stable Annotation semantic targets;
- include previous/next annotation actions and Vim bindings;
- make the behavior work in fixtures and whenever an annotation producer exists;
- return a clear unavailable reason when no annotations exist.

It must not invent a new model annotation protocol or parse arbitrary Markdown conventions merely to make the keybinding appear active. A first-class annotation producer/transport can be a later project.

## 7. Pre-implementation baseline

The design was grounded against `903c377d79d77ab303309b6479f20cbcb4253c02`.
These facts explain the seams the implementation extended; they are historical
baseline context rather than a description of the completed tree.

### 7.1 Existing root and screens

- app/src/desktop.rs contained MapleApp and the Screen enum.
- MapleApp owned Login, Chat, Settings, and a parked Chat entity while Settings was open.
- app/src/backend.rs owned the private Tokio runtime and runtime-effect facade.
- app/src/settings.rs persisted AppSettings safely with serde defaults and a serialized background writer.
- app/src/ui/settings.rs had five fixed sections before Keyboard Shortcuts and Programmability were added.

The completed action host and application semantic controller are rooted at
MapleApp so they operate across Chat, Settings, Login, overlays, and the parked
Chat screen.

### 7.2 Existing GPUI actions and keymaps

- app/src/desktop.rs declares QuitApp and hard-codes app keybindings.
- app/src/ui/chat/mod.rs declares the existing chat actions and attaches listeners to the Chat key context.
- app/src/ui/text_input.rs declares editing actions and separately installs TextInput bindings.
- Current contexts are broad: Chat, Transcript, RootMenu, and TextInput.
- PickQuestionOption is parameterized but currently no_json, so it cannot be dynamically built or discovered through a JSON schema.

GPUI 0.2.2 already supplies App::bind_keys, App::clear_key_bindings, App::build_action, action schemas/documentation, typed dispatch, Window::available_actions, binding lookup, context stacks, and pending keystroke inspection. Window::available_actions is structural/default-buildable availability, not Maple's complete parameterized semantic catalog or business availability; the harness adds that layer explicitly.

Important reload rule: App::clear_key_bindings clears every binding, including TextInput defaults. A profile reload must compile and reinstall the complete resolved keymap atomically.

### 7.3 Existing stable state

ChatScreen already has:

- stable session IDs;
- AgentTimelineItem IDs;
- timeline_index mapping item IDs to current index and revision;
- separate transcript and sidebar ListState values;
- explicit follow_transcript state;
- selection/reload generation fences;
- per-session timeline revisions;
- stable queue IDs, request IDs, and draft image IDs in several paths.

Both transcript and sidebar are virtualized. Off-screen rows have no durable GPUI focus handle. Semantic selection therefore must be application state keyed by stable IDs.

### 7.4 Existing model tool seam

MapleDeveloperClient in crates/maple-agent/src/agent/developer_tools.rs owns the built-in developer tool catalog. Its call_tool method already receives:

- the source session ID;
- the session working directory;
- the current model run CancellationToken.

That is the correct seam for a python_code model tool.

A task's authoritative identity is the Goose session ID plus account scope. Its canonical project root comes from durable session metadata such as AgentSessionSummary.project_root. Never use the currently selected ChatScreen.project_root for a background task's Python kernel.

### 7.5 Existing process-management precedent

The bounded shell implementation already demonstrates:

- process groups on Unix;
- job objects on Windows;
- bounded output;
- timeouts;
- cancellation;
- descendant cleanup;
- careful drain behavior.

Reuse those patterns for Python worker supervision instead of inventing a weaker child-process lifecycle.

## 8. Implemented module boundaries

The dependency direction is the durable contract. This is the implemented
module map; individual files may still move without changing that contract.

~~~text
crates/maple-harness/
  src/action.rs          wire-safe action and descriptor types
  src/registry.rs        validated registry and descriptor lookup
  src/policy.rs          controller policy and host origins
  src/semantic.rs        semantic targets, snapshot, events, revisions
  src/audit.rs           bounded/redacted audit records
  src/keymap.rs          profile-independent keymap data and validation

crates/maple-code-mode/
  src/interpreter.rs     deterministic interpreter discovery/probing
  src/protocol.rs        framed worker protocol
  src/process.rs         child lifecycle and fixed runtime materialization
  src/kernel.rs          per-task actor and state
  src/service.rs         kernel pool and limits
  src/authority.rs       normalized session authority tuple
  src/limits.rs          protocol/output/runtime bounds
  src/controller.rs      UiControllerTransport trait
  python/worker.py       fixed worker bootstrap
  python/maple_gpui/     generated/thin SDK

crates/maple-agent/
  existing developer tool integration
  Maple-owned embedded maple-ui-controller skill

app/src/harness/
  catalog.rs             descriptors and GPUI adapter registration
  host.rs                one executor path, policy, audit, and operations
  adapters.rs            typed GPUI action adapters
  provenance.rs          bounded direct-user input capabilities
  semantic.rs            application semantic controller/projection
  keymap.rs              Standard/Vim templates and context compilation
  keymap_runtime.rs      resolved-map installation and live reload
  shortcut_store.rs      serialized profile/override persistence
  palette.rs             root command palette
  which_key.rs           pending-chord help overlay
  application_vim.rs     application Vim controller
  controller.rs          bounded GPUI request/response bridge

app/src/ui/
  settings/shortcuts.rs  shortcut settings projection and recorder
  settings/programmability.rs
                        Settings-only Code Mode/controller lifecycle state
  chat/code_mode.rs      retained command/lifecycle integration; not mounted
                        as a chat REPL or HUD
  text_input/vim.rs      pure composer Vim engine
~~~

Rules:

- maple-harness has no GPUI dependency.
- maple-code-mode has no GPUI, account client, credential store, or action implementation.
- maple-agent does not depend on app.
- app implements the final GPUI/action host and the UiControllerTransport.
- Python never imports or loads Rust/GPUI internals.
- The external protocol and Python SDK use stable semantic action IDs, not Rust type names.

## 9. Typed semantic action system

### 9.1 Stable IDs

Use validated lowercase dotted IDs that survive Rust refactors:

- app.quit
- settings.open
- task.new
- task.open
- task.set_archived
- project.set_pinned
- sidebar.set_collapsed
- transcript.focus_next
- timeline.set_tool_expanded
- composer.send
- permission.respond
- account.sign_out

Do not expose names such as chat::NewTask as the permanent Python/model protocol. The registry maps a stable external ID to its GPUI adapter and one executor.

ID validation should reject empty segments, uppercase characters, whitespace, and ambiguous aliases. Descriptors have a schema version and may later carry deprecated stable-ID aliases.

### 9.2 Core types

The implementation should express at least these concepts:

~~~rust
pub struct ActionDescriptor {
    pub schema_version: u16,
    pub id: ActionId,
    pub label: String,
    pub description: String,
    pub category: String,
    pub argument_schema: serde_json::Value,
    pub result_schema: serde_json::Value,
    pub contexts: Vec<SemanticContextPattern>,
    pub effect: ActionEffect,
    pub invocation_policy: InvocationPolicy,
    pub recoverability: Recoverability,
    pub audit: AuditSpec,
    pub default_bindings: DefaultBindings,
}

pub enum ActionEffect {
    Observe,
    Navigate,
    MutateMaple,
    ExternalEffect,
}

pub enum InvocationPolicy {
    ControllerCallable,
    HumanOnly,
}

pub enum Recoverability {
    Ephemeral,
    Reversible,
    Irreversible,
}

pub struct ActionCall {
    pub action_id: ActionId,
    pub arguments: serde_json::Value,
    pub target: Option<SemanticTarget>,
    pub precondition: Option<ActionPrecondition>,
}

pub struct ActionPrecondition {
    pub domain: RevisionDomain,
    pub target_revision: u64,
}

pub struct ActionResponse {
    pub invocation_id: InvocationId,
    pub action_id: ActionId,
    pub status: ActionStatus,
    pub result: Option<serde_json::Value>,
    pub error: Option<ActionError>,
    pub state_revision: Option<u64>,
}
~~~

Action errors must be structured:

- unknown_action;
- invalid_arguments;
- not_applicable;
- unavailable;
- policy_denied;
- stale_target;
- cancelled;
- failed.

An unavailable response includes a stable reason code and human-readable explanation. Current handlers often silently return while busy or when a target is absent; that is insufficient for a command palette or model.

### 9.3 Trusted invocation context

The wire request contains only ActionCall. It cannot choose its own actor, transport, controller mode, account, task, program/run, cancellation token, or policy epoch.

The host constructs a non-deserializable invocation:

~~~rust
pub struct TrustedInvocation {
    pub invocation_id: InvocationId,
    pub source_task: Option<TaskIdentity>,
    pub program_id: Option<ProgramId>,
    pub model_run_id: Option<RunId>,
    pub actor: InvocationActor,
    pub transport: InvocationTransport,
    pub controller_access: UiControllerAccess,
    pub policy_epoch: u64,
    pub cancellation: CancellationToken,
}

pub enum InvocationActor {
    DirectUser,
    Model,
    UserCode,
    Internal,
}

pub enum InvocationTransport {
    Pointer,
    Keybinding,
    CommandPalette,
    Python,
    Macro,
    GeneratedUi,
    Internal,
}
~~~

Actor and transport are separate because model-via-Python and a future trusted
manual Python surface have different provenance while sharing a transport.
Model-triggered Python has actor Model and transport Python. The reserved
UserCode actor does not gain DirectUser authority. Human Only means actor
DirectUser through a private, approved Pointer, Keybinding, or CommandPalette
entry point—not code that claims a human asked it to act.

Never infer DirectUser merely because a GPUI action handler ran. App::dispatch_action is callable programmatically and GPUI handlers do not prove physical input. Only private UI-host entry points handling an actual pointer event, a resolved physical key event, or explicit command-palette activation may mint a direct-user invocation token. Adapter constructors and those tokens are not public to model, Python, macros, generated UI, or arbitrary internal callers. Add an adversarial test proving programmatic GPUI dispatch cannot invoke Human Only.

Multi-stroke key resolution needs provenance even when GPUI dispatches a shorter complete binding after its prefix timeout and no physical event is currently on the stack. The window host creates a non-forgeable, window-scoped PendingKeyProvenance token on the first physical keystroke, carries it only through GPUI's current pending sequence, and consumes it when the resolved keybinding adapter dispatches. Focus/context change, explicit cancellation, timeout with no resolved binding, recorder takeover, or sequence completion clears it. A timeout-resolved shorter binding consumes the surviving token and remains DirectUser/Keybinding; App::dispatch_action without that private token does not.

Internal is not inherently privileged. Derived/internal follow-up actions inherit the initiating actor, transport, controller access, policy epoch, cancellation, program ID, and model run ID. Truly autonomous maintenance operations require a narrow compile-time allowlist and cannot resolve or activate user-selected Human Only actions.

The Human Only allow matrix is exhaustive: only (DirectUser, Pointer), (DirectUser, Keybinding), and (DirectUser, CommandPalette) qualify. Reject every other actor/transport tuple, including every Internal combination. Autonomous allowlisted maintenance never invokes or indirectly activates Human Only.

### 9.4 One-path invariant

The semantic dispatcher owns the only action executor.

~~~text
button ----------------------+
typed GPUI key action -------+
command palette -------------+--> semantic dispatcher --> executor
model/Python request --------+
future generated surface ----+
~~~

GPUI actions are typed transport adapters into the dispatcher. They are not a parallel business-logic implementation. Maple's semantic registry enriches GPUI actions; it does not compete with GPUI's action catalog. Every bindable semantic action has a one-to-one typed GPUI adapter whose stable external identity maps to the semantic descriptor, and each adapter delegates to the same executor. Do not replace this with one generic Invoke(String, Value) GPUI action: per-action schemas, contexts, keymap discovery, and documentation must remain visible.

GPUI is the typed input adapter, not the result channel. The semantic executor owns ActionResponse and asynchronous completion. Human GPUI dispatch may discard a response after audit and UI error handling; controller calls reach the same executor through a bounded request/oneshot bridge. Do not infer operation completion from Window::dispatch_action's return.

Examples of existing divergence that must disappear:

- The New Task shortcut and sidebar New Task button currently reach new_session through different callbacks.
- The Toggle Sidebar shortcut and header button mutate the same state through different code.
- The Open Settings shortcut and sidebar gear emit through different callbacks.
- Settings controls directly toggle fields without a discoverable typed action.

Buttons should dispatch a typed semantic action or call the same dispatcher with a host-created DirectUser/Pointer context. The keymap compiler maps stable action IDs and typed arguments to GPUI actions. Model and Python calls validate the same argument schema and enter the same executor directly.

### 9.5 Parameterized actions

Every bindable/programmatic parameterized action must support serde deserialization and a JSON schema. Remove no_json from actions such as PickQuestionOption when they enter this system.

Result schemas matter too. A model program needs to know whether task.new returns a task ID, settings.open returns a semantic screen target, or an async action returns an accepted run identifier.

### 9.6 Setters over toggles

Programmatic actions should be deterministic:

- task.set_archived with task_id and archived;
- project.set_pinned with root and pinned;
- sidebar.set_collapsed with collapsed;
- composer.set_expanded with expanded;
- task.set_web_enabled with task_id and enabled;
- mcp.set_enabled with server_id and enabled.

Human convenience toggle actions may remain, but should resolve current state and delegate to the setter. Python/model flows must not rely on stale toggles.

### 9.7 Availability

Each action has a side-effect-free availability function evaluated against current semantic state and arguments. It returns:

~~~rust
pub enum Availability {
    Available,
    Disabled {
        code: DisabledReasonCode,
        message: String,
    },
}
~~~

Availability is not authority. An action can be applicable but denied by policy. It is also not GPUI structural availability alone. A task action may be available for an off-screen stable target even though no row is rendered.

### 9.8 Revisions and asynchronous effects

State snapshots and semantic events carry monotonically increasing global revisions for snapshot coherence and event sequencing. Programmatic calls may also supply an action-declared target revision or precondition returned with the queried target. Compare only the revision domain relevant to that action—such as a task record, draft, setting, or timeline item. Do not reject an unrelated task-open or settings call merely because a streaming transcript advanced the global state revision. If the relevant target changed, return stale_target rather than operating on a surprising object.

For async actions:

- accepted is not completed;
- the response must expose a run/invocation ID when work continues;
- audit completion occurs when the actual result is known;
- cancellation may prevent later stages but does not pretend to reverse completed effects;
- a policy lease is rechecked immediately before a deferred effect commits.

### 9.9 Registry validation

Startup/tests must fail for:

- duplicate or invalid stable IDs;
- missing labels/descriptions;
- bindable actions without argument schemas;
- missing result schemas for controller-callable results;
- missing GPUI adapters;
- keymap references to unknown IDs;
- default bindings that cannot be parsed;
- unsafe audit defaults on secret-bearing actions.

## 10. Authority model

### 10.1 Controller modes

UiControllerAccess is a new type and must not reuse the existing agent PermissionMode.

| Controller mode | Observe | Navigate | Mutate Maple | External effect | Human Only |
|---|---:|---:|---:|---:|---:|
| Off | deny | deny | deny | deny | deny |
| Read Only | allow | allow | deny | deny | deny |
| Full Access | allow | allow | allow | allow | deny |

Direct-user pointer, keybinding, and command-palette invocations are not constrained by the controller mode. They still obey ordinary action availability.

Full Access intentionally has no per-action confirmation prompts for ordinary controller-callable actions. It is meant to let the model chain useful workflows. The permanent Stop control and audit provide visibility and revocation.

### 10.2 Initial Human Only set

At minimum:

- account.sign_out;
- account deletion;
- email, password, recovery, MFA, credential, token, and session-revocation operations;
- authentication submission/confirmation operations;
- controller mode and Code Mode authority changes;

Controller and Code Mode authority changes are direct-human-only because:

- a Read Only controller must not promote itself to Full Access;

Settings and MCP mutations are ordinary Full Access actions. Sending a message
to another task is an ordinary Full Access action. Viewing or navigating tasks
is Read Only. `permission.respond` is also an ordinary Full Access mutation,
including for a request associated with the originating model run. This is an
explicit product-policy choice for review: permanent account/identity
operations and authority changes remain Human Only. Keep the agent permission
setting separate, display and audit controller-originated responses clearly,
and do not invent a self-approval prohibition without a later product decision.

app.quit remains controller-callable in Full Access. It is disruptive but not a permanent account-level effect. Mark its descriptor as a terminal host action. Commit the accepted audit record, queue the terminal SDK response, stop new admission, and only then begin orderly shutdown. The caller may observe accepted_terminal rather than a normal post-shutdown completion. Cancellation before admission prevents it; once shutdown is committed it is not reversible.

### 10.3 Final-action reauthorization

Generic actions such as ui.activate_selected are useful, but they are never authority shortcuts.

The dispatcher must:

1. Resolve the selected semantic object.
2. Resolve its concrete backing action and arguments.
3. Re-evaluate availability.
4. Re-read the current controller policy and epoch.
5. Apply the concrete action's Human Only/effect policy.
6. Invoke the concrete executor.

Selecting Sign Out and then calling ui.activate_selected must still be denied to Python/model origins.

### 10.4 Policy revocation

Policy is revisioned.

- Turning Python Code Mode Off cancels active executions and stops kernels.
- Downgrading Full Access to Read Only revokes pending mutation leases.
- Turning the controller Off revokes all SDK requests and event waits.
- A previously imported maple_gpui module remains powerless because Rust checks current policy on every request.
- A stale model tool call offered before settings changed fails closed at execution time.

## 11. Semantic control migration

There are roughly seventy-six current on_click sites. Not all are semantic, but every discoverable, bindable, auditable, or programmatically useful result must enter the registry.

### 11.1 Initial catalog

| Area | Required action family |
|---|---|
| App/screens | app.quit, settings.open, settings.close, settings.open_section, shortcuts.open, code_mode.open |
| Account/auth | account.sign_out and current sign-in/OAuth operations as Human Only |
| Tasks | task.new, task.open, task.rename, task.set_archived, task.focus_previous, task.focus_next |
| Projects | project.choose, project.switch, project.set_collapsed, project.set_pinned, project.rename, project.reveal, project.remove, project.set_trusted |
| Sidebar | sidebar.focus, sidebar.set_collapsed, sidebar.clear_search, sidebar.set_archived_expanded |
| Transcript | transcript.focus, focus_next, focus_previous, focus_first, focus_last, copy_selection, select_all |
| Timeline | timeline.copy_item, timeline.set_tool_expanded, timeline.open_attachment, timeline.toggle_speech |
| Composer | composer.focus, set_text, send, steer, set_expanded, attach_files, remove_attachment, set_model, set_permission_mode, set_web_enabled, set_mcp_enabled, toggle_recording |
| Text input | every Standard/Vim keymap primitive, including movement, selection, deletion, clipboard, undo/redo, newline, and submit actions |
| Composer Vim | typed command families for motion, operator, count digit, text object, insert entry, Visual, paste, undo/redo, repeat, and cancel |
| Runs/queue | run.stop, queue.steer, queue.begin_edit, queue.remove, queue.discard_edit |
| Questions | question.select_option, question.submit, question.skip |
| Permissions | permission.respond as a Full Access mutation; selection remains Read Only |
| Settings | explicit setter for every persisted preference; prompt save/reset; MCP add/update/set_enabled/remove |
| Utilities | ui.dismiss, ui.reveal, link.open, clipboard.copy |
| Code Mode | code_mode.execute, stop, reset, restart, clear_scratch, set_enabled, set_controller_access |

The agent must inventory current controls against this table and current source, not assume the table is exhaustive if upstream added a control. Every GPUI action referenced by Standard, Vim, or a user keymap—including TextInput primitives and composer Vim command tokens—has a semantic descriptor and appears in shortcut discovery. Focus-local editor primitives may be Human Only and shortcut-only rather than controller-callable, but they cannot live in an undocumented second action universe.

### 11.2 Stable action arguments

Never expose:

- sidebar or timeline vector indices;
- GPUI Entity or FocusHandle values;
- render-only element IDs;
- the currently visible card as an implicit model target;
- draft attachment array indices.

Use:

- task/session IDs;
- canonical project roots or stable project IDs;
- timeline item IDs;
- permission/question/request IDs;
- queue IDs;
- DraftImage.id;
- stable settings keys;
- menu IDs plus item IDs.

### 11.3 Local gesture allowlist

These may remain local mechanics:

- drag-selection position updates;
- mouse hit testing;
- hover;
- scrollbar/wheel movement;
- layout measurement;
- cursor painting;
- event propagation wrappers.

The semantic result still becomes an action. For example, drop hit testing is local, but attaching the resulting file list is composer.attach_files. A backdrop click is local input, but its result is ui.dismiss.

Keep a checked-in control inventory with, for every inspected control, its source path/symbol, stable action ID and executor, or explicit local-gesture exemption reason. Add a source-audit test/script which inventories direct semantic on_click and comparable activation callbacks in the relevant UI modules and fails on an unexplained site. The script need not solve arbitrary Rust dataflow; a required adjacent semantic-action/local-gesture marker plus registry validation is sufficient. Review fails on an unexplained direct semantic callback.

Registry tests must also prove stable action ID to App::build_action to the typed GPUI adapter to the semantic executor. GPUI all_action_names enumerates registered action types and Window::available_actions describes structurally available/default-buildable actions; neither is, by itself, the complete parameterized Maple semantic catalog.

### 11.4 Root ownership

MapleApp should own or coordinate the dispatcher because it can route to:

- current ChatScreen;
- parked ChatScreen while Settings is open;
- SettingsScreen;
- LoginScreen;
- root overlays;
- application shutdown.

Screen-specific executors can remain methods on those entities. They are reached only through the root dispatch table.

### 11.5 Native argument acquisition

Actions that need a human-selected path still use the same semantic action
path. `project.choose` and an empty-path `composer.attach_files` request acquire
their arguments through GPUI's native directory or multi-file picker inside
the owning executor; a cancelled picker completes as a successful no-op.
Explicit controller-supplied paths do not open a picker. Project selection may
fall back to the existing Linux portal/manual-entry flow when the native
directory picker is unavailable. Picker acquisition does not bypass policy,
provenance, operation completion, or cancellation barriers.

## 12. Semantic UI model

### 12.1 Five independent concepts

Do not collapse these into one focus field:

1. GPUI keyboard focus: which rendered element receives events.
2. Semantic selection: which stable application object is selected by application Vim or model navigation.
3. Text insertion/selection: byte/grapheme positions in a TextInput or rich-text selection.
4. Viewport: which virtual rows happen to be visible.
5. Stream follow: whether appended transcript output keeps the viewport at the bottom.

Streaming may update a selected timeline item's revision without changing its semantic ID. Revealing a semantic selection may move the viewport without changing text insertion. Focusing a TextInput may move GPUI focus without selecting a transcript item.

### 12.2 Semantic target types

Use a serializable enum internally and on the controller wire:

~~~rust
pub enum SemanticTarget {
    App,
    Screen { screen: ScreenId },
    Region { region: RegionId },
    Project { canonical_root: String },
    Task { task_id: String },
    TimelineItem { task_id: String, item_id: String },
    Annotation {
        task_id: String,
        item_id: String,
        annotation_id: String,
    },
    Permission { request_id: String },
    Question {
        request_id: String,
        question_id: String,
    },
    QueueItem { task_id: String, queue_id: String },
    DraftAttachment { draft_id: u64 },
    Setting { key: String },
    MenuItem { menu_id: String, item_id: String },
}
~~~

Use structural serialization on the wire. A display path such as:

~~~text
task:<id>/timeline:<item-id>/annotation:<annotation-id>
~~~

is useful for logs and UI, but should not require fragile string parsing inside executors.

### 12.3 Semantic snapshot

ui.describe returns a redacted, versioned projection rather than Rust state:

~~~rust
pub struct SemanticSnapshot {
    pub schema_version: u16,
    pub state_revision: u64,
    pub event_cursor: u64,
    pub screen: ScreenId,
    pub active_region: RegionId,
    pub selection: Option<SemanticTarget>,
    pub insertion: Option<InsertionSummary>,
    pub viewport: ViewportSummary,
    pub stream_follow: bool,
    pub roots: Vec<SemanticNode>,
}

pub struct SemanticNode {
    pub target: SemanticTarget,
    pub kind: SemanticKind,
    pub label: Option<String>,
    pub state: SemanticNodeState,
    pub available_actions: Vec<ActionAvailabilitySummary>,
    pub children: Vec<SemanticNode>,
}
~~~

The snapshot may include task titles and visible transcript semantics needed for navigation. It must never include:

- passwords;
- OAuth callbacks/tokens;
- secret MCP headers or environment values;
- raw account credentials;
- hidden permission payloads;
- database handles or paths unrelated to the current product view.

Redaction applies in Read Only and Full Access.

### 12.4 Semantic selection state

Recommended application state:

~~~rust
pub struct SemanticSelection {
    pub region: RegionId,
    pub target: Option<SemanticTarget>,
    pub anchor: Option<SemanticTarget>,
    pub explicit: bool,
    pub observed_revision: u64,
}
~~~

Keep a bounded region-return stack, not one overwrite-prone previous slot. Each frame records screen, region, stable target, reason, and relevant revision. Entering a transient overlay/dialog pushes; closing it pops exactly that frame. Moving from an application region into the composer pushes the originating region once; repeated composer mode changes do not keep pushing. Composer Normal Escape pops back to that region. gi stores/restores the composer insertion separately and does not pop an unrelated overlay frame.

If a stored target no longer exists, restore the region and its nearest valid selection. With no prior application region, composer Escape falls back to Transcript in Chat. gi is unavailable outside Chat. Its per-task insertion point carries the draft revision; after draft/task replacement, validate and clamp it to a grapheme boundary, or fall back to the end of the current draft rather than using a stale byte offset.

### 12.5 Virtualized transcript rules

Store transcript selection as task ID plus timeline item ID, never as row index.

Each region exposes one ordered navigable-child projection separate from its raw backing collection. j/k/gg/G and semantic query use that projection. A child is eligible only when it has a stable target and a visible/revealable rendering with nonzero semantic presence. Skip internal todo/state items, filtered objects, empty placeholders, zero-height rows, and implementation-only records unless they deliberately render an accessible semantic object. Ordering matches visible application order. "Newest" means the last eligible projected object; "newest assistant" means the last eligible assistant message object, including a currently streaming assistant message once its semantic row exists. Tests must include hidden and zero-height backing items so selection can never land on an object with no highlight or reveal target.

Resolve through timeline_index immediately before:

- reveal;
- copy;
- activate;
- move to neighboring item;
- find annotations/assistant items.

Reconciliation:

- Same item ID with a newer content revision: retain selection.
- Items inserted before the target: retain target and resolve its new index.
- Selected item removed: choose the nearest surviving neighbor using the previous ordered-ID list; then fall back to the region itself.
- Task switch: restore that task's prior semantic selection if still valid.
- Selecting away from newest: set follow_transcript false.
- Explicit G or jump-to-latest: select/reveal newest and re-enable follow.
- New chunks never implicitly re-enable follow.
- Filtering may hide a valid target without deleting it; reveal may explicitly clear the filter.

Do not splice/re-measure the newest streaming item in a way that regresses the repository's existing wheel-scroll protection.

### 12.6 Sidebar, settings, menus, questions, and permissions

- Sidebar task rows resolve by task ID.
- Project headers resolve by canonical root.
- Settings rows resolve by stable setting/action key.
- Menus resolve by stable menu ID plus item ID.
- Questions and permissions resolve by request IDs.
- Selecting a permission is Read Only navigation.
- Responding to it is a separate mutation: direct users may invoke it, Read Only cannot, and Full Access controllers may invoke it under the explicit policy in Section 10.2.

### 12.7 Semantic events

Expose a bounded, sequence-numbered event stream:

- screen_changed;
- region_changed;
- semantic_selection_changed;
- target_updated;
- target_removed;
- task_opened;
- action_started;
- action_completed;
- controller_policy_changed;
- code_mode_state_changed.

events.wait accepts a predicate, an after cursor, and a timeout. This prevents the classic query-then-wait race:

1. ui.describe returns event_cursor N.
2. Invoke an action.
3. Wait after N.

If the bounded event buffer overflows, return a resync_required error and require a fresh ui.describe. Do not encourage polling loops.

## 13. Shortcut profiles and keymap format

### 13.1 Profile semantics

Use these first-party profiles:

- Standard
- Vim

The resolver is:

~~~text
selected Standard OR Vim template
                |
                v
          user overrides
                |
                v
      one complete GPUI keymap
~~~

Do not install Standard and then overlay Vim. Vim replaces Standard so its modal grammar does not fight ordinary shortcuts. Replacement does not mean ordinary controls stop working: both templates independently include the complete essential keymap for every non-composer input role. If Vim wants Secondary-C in Insert, or ordinary editing in search/login/rename/settings, those bindings must be explicit in the Vim template.

Changing profile must:

1. Parse/validate the target template and overrides.
2. Build a complete resolved map, including TextInput essentials.
3. If valid, clear and reinstall all GPUI bindings atomically.
4. Update key contexts and modal state.
5. If invalid, keep the last-known-good map and show errors.

### 13.2 Persistence

Persist the selected profile and simple preferences in settings.json:

~~~json
{
  "keymap_profile": "vim",
  "vim_leader": "space"
}
~~~

Keep editable overrides under the platform config root documented in README:

~~~text
<config>/keymap.json
~~~

Use a Zed-style JSON array:

~~~json
[
  {
    "context": "MapleApp && profile == vim && region == transcript && app_vim_mode == normal",
    "bindings": {
      "j": "transcript.focus_next",
      "k": "transcript.focus_previous",
      "space s n": "task.new",
      "secondary-k": null,
      "ctrl-enter": [
        "composer.send",
        { "steer": true }
      ]
    }
  }
]
~~~

Rules:

- A string is an action with default/no arguments.
- An array is a stable action ID plus typed argument object.
- null disables that sequence in the matching context.
- A sequence is space-separated GPUI keystrokes.
- Context is a GPUI-compatible boolean predicate.
- Unknown fields and invalid action arguments produce surfaced diagnostics.
- Never silently rewrite or discard an invalid hand-edited file.
- Settings UI edits write atomically and preserve a useful formatted representation.

### 13.3 Precedence and conflict model

Within one active context:

- More specific/deeper matching contexts win.
- At equal specificity, later user entries win.
- User overrides load after the selected template.
- null is a real high-precedence unbinding.

The resolver retains a ResolvedBinding side table with:

- source template/file and source location;
- context string and parsed predicate;
- key sequence;
- stable action ID and arguments;
- default binding;
- effective/shadowed/disabled state;
- exact/prefix/possible conflicts.

Conflict detection covers:

- identical sequences with overlapping contexts;
- one sequence being a prefix of another;
- duplicate user entries;
- invalid IDs or arguments;
- impossible action/context combinations;
- deliberate user shadowing of a template.

Arbitrary context-overlap proof is difficult. When disjointness cannot be proven, report a possible conflict instead of pretending certainty.

Prefix bindings are valid. Follow GPUI/Zed behavior: wait briefly for a longer sequence when a shorter complete binding is also a prefix.

### 13.4 Context tree

Enrich the existing contexts. A representative tree is:

~~~text
MapleApp os=macos profile=vim screen=chat app_vim_mode=inactive
  Chat
    Sidebar region=sidebar app_vim_mode=normal popup=none
    Transcript region=transcript app_vim_mode=normal popup=none
    Composer region=composer app_vim_mode=inactive
      TextInput input_role=composer editor_vim_mode=insert
    Overlay overlay=command_palette
    Dialog dialog=question
~~~

Other input roles include:

- password;
- login;
- search;
- rename;
- settings;
- prompt;
- question;
- mcp_editor;
- code_editor.

Application and editor modes are separate keys. Never reuse a broad vim_mode=normal predicate for Sidebar, Transcript, and composer Normal. app_vim_mode is normal only while the application controller owns unmodified navigation; editor_vim_mode is normal/insert/visual only on the opted-in composer. Overlays and dialogs add higher-priority contexts. Do not depend on render-updated key-context fields for operator/count/text-object grammar between rapid keystrokes; the focused editor action handler resolves contextual command tokens directly against its current VimState.

Only input_role=composer receives the composer Vim engine in the first implementation. Context-specific application bindings must not leak into other inputs. Every ordinary input role retains its complete TextInput essentials while the Vim profile is active, including cursor movement, selection, deletion, clipboard, undo/redo, IME, submit/cancel, and its surface-specific keys.

### 13.5 Standard template

Preserve current ordinary behavior, including platform modifier variants and TextInput editing. Add discoverable defaults such as:

- Secondary-Shift-P: command palette (Cmd-Shift-P on macOS, Ctrl-Shift-P elsewhere).
- Secondary-K Secondary-S: shortcut settings.
- Existing New Task, search, sidebar, archived, settings, project, permission, transcript copy/select-all, menu, and TextInput bindings.

The exact existing shortcuts should be migrated, not casually changed. The Standard template includes ordinary TextInput essentials for all input roles.

### 13.6 Vim template

The Vim profile has its own complete template:

- Application Normal commands described below.
- Composer modal bindings described below.
- Explicit platform editing bindings in Insert where desired.
- Colon command palette.
- Space leader.
- Ctrl-W region movement.
- Ordinary TextInput essentials for input_role != composer, independent of the Standard template.

GPUI resolves every remappable stroke and static multi-stroke chord. Application Vim owns semantic selection and count state, but it does not run an independent raw g/bracket/leader chord parser. Composer Vim receives typed command/token actions produced by the effective keymap; it does not hard-code physical h/j/k/l/d/etc. Count digits are typed tokens; the state-aware editor resolver applies the context-sensitive 0 rule synchronously, so the shortcut registry can display and remap them without waiting for a render. The only raw composer interception is the final safety/IME guard that prevents an unmatched printable key from inserting in Normal/Visual and cancels invalid pending grammar.

Secondary-J/Secondary-K may be user-configurable aliases but are not foundational defaults. The core spatial grammar is Ctrl-W h/j/k/l. This avoids importing tmux shortcuts literally and keeps key choices editable.

## 14. Shortcut settings UX

Add a Keyboard Shortcuts settings section generated from the semantic registry.

### 14.1 Page layout

Header:

- Search field accepting label, stable ID, description, category, or keystroke.
- Profile dropdown: Standard or Vim.
- Current state: Default or Modified.
- Reset Profile.
- Open keymap.json.
- Filters: All, Conflicts, Modified, Unbound.

Each action row shows:

- human label;
- stable action ID;
- description;
- category;
- supported semantic contexts;
- effect classification;
- current bindings;
- selected-template defaults;
- conflict badges;
- unbound/disabled status;
- unavailable reason when evaluated in the current screen;
- Record/Add;
- Disable;
- Reset action.

Parameterized actions show their bound arguments. The page itself is keyboard navigable and participates in application Vim.

### 14.2 Shortcut recorder

The recorder:

- activates only after a direct-user click/command;
- intercepts keystrokes and stops normal dispatch while recording;
- records a bounded multi-stroke sequence;
- displays the sequence live;
- Enter commits;
- Escape cancels;
- Backspace removes the latest stroke;
- Clear creates a null override when requested;
- shows exact, prefix, and possible conflicts before commit;
- lets the user replace, keep both with a narrower context, or cancel.

Use GPUI keystroke interception. Do not make the recorder a global permanent interceptor.

### 14.3 Reset behavior

- Reset action removes overrides affecting that action and reveals the current template binding.
- Disable writes an explicit null mapping.
- Reset Profile removes all overrides only after direct-user confirmation in the settings UI.
- Switching templates does not delete user overrides; inactive-context overrides remain visible.

Named custom profiles can wait. Standard/Vim plus arbitrary overrides are sufficient initially.

## 15. Command palette and which-key

### 15.1 Root command palette

This is distinct from the current composer slash-command palette.

It searches:

- action labels;
- stable IDs;
- descriptions;
- categories;
- current keybindings;
- optional Ex-style aliases.

It shows:

- current availability and disabled reason;
- action effect/recoverability;
- current shortcut;
- inferred semantic target;
- argument UI for the small number of actions that cannot infer arguments.

Invocation is a trusted DirectUser/CommandPalette call to the same dispatcher.

Defaults:

- Secondary-Shift-P in Standard.
- colon in application Vim Normal.

The first colon layer can provide aliases such as:

- settings;
- shortcuts;
- task new;
- project open;
- code;
- quit.

This is not a promise of a full Vim Ex parser. Colon primarily opens and filters the real action palette.

### 15.2 Which-key

Compile effective bindings into a prefix trie.

When GPUI reports pending multi-stroke input:

- wait roughly 250 to 400 ms so fast sequences do not flash;
- filter continuations against the current context stack;
- show next keys, labels, and action IDs;
- include the configurable Space leader;
- disappear on completion, cancellation, focus/context change, or timeout.

The first implementation is a passive preview. GPUI exposes pending input but not a public arbitrary clear operation; do not replace GPUI's dispatcher with a clickable which-key engine.

## 16. Application Vim

### 16.1 Ownership

Application Vim is a controller over semantic actions. It does not synthesize keys and does not reuse the composer editing engine.

It owns:

- active semantic region;
- selected semantic target;
- bounded region-return stack;
- count prefix;
- current application mode/context.

GPUI owns remappable single- and multi-stroke resolution for g, bracket, Ctrl-W, and leader sequences. Application Vim consumes the resulting typed semantic actions. GPUI focus moves only as necessary for keyboard routing. Selection and viewport remain separate.

### 16.2 Required navigation

| Intent | Default Vim command |
|---|---|
| Previous/next semantic item in region | k / j |
| First/last semantic item | gg / G |
| Newest assistant response | ga |
| Previous/next assistant response | left-bracket a / right-bracket a |
| Previous/next annotation | left-bracket d / right-bracket d |
| Return to composer last insertion point | gi |
| Move among adjacent regions | Ctrl-W h/j/k/l |
| Search active surface | slash |
| Open action command line/palette | colon |
| Open application command namespaces | configurable Space leader |
| Activate/open selected object | Enter |
| Collapse/parent or expand/child where meaningful | h / l |
| Close top overlay or return to containing surface | Escape |

Counts apply to j/k and semantic next/previous families where useful.

The d annotation mnemonic deliberately follows Vim's diagnostic navigation convention. ga is an explicit Maple mnemonic for the newest assistant item.

### 16.3 Region behavior

Transcript:

- j/k move among semantic timeline objects, not rendered lines.
- h/l collapse/expand tool/reasoning/details where applicable.
- Enter opens/activates the selected object.
- y copies canonical visible text where a copy action exists.
- G selects/reveals newest and re-enables stream follow.

Sidebar:

- j/k move among task and project rows.
- h collapses or moves to parent.
- l expands or moves to child.
- Enter opens the task/project.

Settings:

- j/k move rows.
- h/l may change segmented options only when that row declares such behavior.
- Enter activates the row or enters its control.

Menus/questions/permissions:

- j/k move options.
- Enter activates the selected safe/direct-user action.
- Escape closes or returns.
- Permission responses remain separate mutations; Full Access controller behavior follows Section 10.2 and is conspicuously audited.

Composer:

- composer-specific Normal commands edit text.
- Ctrl-W movement or ga leaves the composer region through application actions reserved in the composer-Normal keymap context.
- a second Escape from composer Normal returns to the previous application region.

### 16.4 Leader defaults

Space is the default leader and is configurable. Initial namespaces should be small and discoverable:

- Space s: tasks;
- Space p: projects;
- Space a: agents/assistant/annotations;
- Space m: MCPs;
- Space comma: settings;
- Space question-mark: which-key/help.

Examples such as Space s n for task.new are appropriate. Do not fill every possible sequence merely for completeness; the registry and which-key should make additions easy.

### 16.5 Streamed-response experience

When output streams while the composer is active:

- GPUI insertion focus does not move.
- Composer cursor/selection does not move.
- An explicit transcript selection does not move.
- If the viewport was following newest content, it may continue following.
- If the user navigated away, chunks do not repin it.
- ga explicitly selects the newest assistant object.
- gi returns to the composer and enters Insert at its stored insertion point
  when the draft revision still matches, or at the end of a replaced draft.

### 16.6 Application object operators

Do not assign generic d/c/p mutations across application objects until each region has an explicit safe meaning. For this milestone:

- navigation, activation, reveal, and copy are required;
- mutations remain named actions available through palette/leader/keymap;
- a future dd for task archive must call task.set_archived and preserve its action policy.

This avoids making d mean deletion on one screen, archive on another, and permission denial on a third.

## 17. Composer Vim

### 17.1 Scope and integration

Implement a pure editor engine in app/src/ui/text_input/vim.rs. TextInput holds Option<VimState>.

Provide an opt-in builder and live setter:

~~~rust
TextInput::new(...).vim_enabled(true);

input.set_vim_enabled(enabled, cx);
input.vim_mode() -> Option<VimMode>;
input.vim_status() -> Option<VimStatus>;
~~~

Only the main chat composer opts in initially. Password, login, sidebar search, rename, question, prompt, settings, MCP editor, and other TextInput instances remain ordinary text inputs.

Disabling Vim at runtime:

- cancels editor count/operator/text-object pending state;
- invalidates the window PendingKeyProvenance and keymap generation so a delayed GPUI g/leader/bracket prefix adapter from the old profile is rejected even if GPUI has not exposed an explicit clear-pending API;
- exits Visual;
- commits or safely terminates any insertion transaction;
- collapses selection to a valid insertion boundary;
- restores current Standard behavior immediately.

### 17.2 State

Required persistent modes:

~~~text
Disabled
Normal
Insert
Visual characterwise
~~~

Orthogonal/transient state:

- count prefix;
- pending operator d/c/y;
- pending text-object prefix i/a;
- Visual anchor and head;
- unnamed register plus characterwise/linewise kind;
- preferred logical column for repeated j/k;
- current insertion undo transaction;
- last structured mutating command for dot;
- last composer insertion location for application gi.

Use UTF-8 byte offsets at the TextInput boundary. Compute character motions on Unicode grapheme boundaries. Never split a scalar, combining sequence, or emoji cluster.

Normal's logical cursor rests on a grapheme. Insert uses a boundary between graphemes. Visual is internally inclusive and converts carefully to TextInput's existing end-exclusive selected_range.

### 17.2.1 Typed Vim command actions

The pure editor is a state machine over typed commands, not raw key identities. Register descriptor-backed, remappable action families such as:

~~~text
composer.vim.motion { motion }
composer.vim.begin_operator { operator }
composer.vim.count_digit { digit }
composer.vim.text_object_prefix { inner_or_around }
composer.vim.text_object { object }
composer.vim.enter_insert { placement }
composer.vim.open_line { above_or_below }
composer.vim.toggle_visual
composer.vim.delete_chars
composer.vim.paste { placement }
composer.vim.undo
composer.vim.redo
composer.vim.repeat
composer.vim.cancel
~~~

The implementation should use ordinary registry descriptors and typed GPUI action structs. Each command is visible/remappable in Shortcut Settings with its editor mode. For keys whose meaning is grammar-dependent, bind a typed contextual token and let the focused engine resolve it synchronously against current VimState in the same handler—for example i means enter-insert normally and inner after an operator, a means append normally and around after an operator, w means motion normally and Word after an i/a text-object prefix, a second d completes a linewise delete, and 0 means line-start or count digit according to stored count. Correctness must not depend on a key-context update, notification, or render occurring between physical keys.

All countable command tokens carry no baked-in count. They consume the engine's accumulated count exactly once, defaulting to one; operator and motion counts are stored separately and multiply. Thus 3x deletes three graphemes rather than multiplying a count embedded in x, and d2d applies the operator/count grammar exactly once.

Static multi-stroke ownership remains in GPUI. In composer Normal, the template resolves:

- g g to the local first-line motion, consuming the editor count;
- g a to transcript.focus_newest_assistant and leaves the composer;
- g i to composer.focus_last_insertion, which enters Insert even when already in the composer;
- left-bracket/right-bracket a/d, colon, slash, Space leader, and Ctrl-W h/j/k/l to application actions;
- operator then motion/text-object as typed commands interpreted by editor state.

Neither GPUI and the editor nor application Vim and the editor may simultaneously own the same pending prefix. Unknown or invalid input while an operator/count/text-object is pending is consumed, cancels the pending grammar, reports a short status, and never inserts text, sends, or propagates to Chat. Escape always cancels pending grammar to composer Normal. Cap a parsed count at a documented constant such as 999_999 and report overflow rather than wrapping. A leading 0 is line-start when no count exists and a digit once a nonzero count exists. d2d is linewise delete with count multiplication.

Editor grammar has no wall-clock timeout: like Vim, a completed d/count/i-or-a prefix waits for its next semantic token until completion, Escape, invalid input, focus loss, task/draft replacement, profile change, or popup takeover. GPUI's timeout applies only to static keymap chords such as gg versus another configured g sequence. Which-key may render editor-pending grammar from VimState separately from GPUI's static pending trie.

### 17.3 Required motions

- h: previous grapheme in the logical line.
- l: next grapheme in the logical line.
- j: next logical line while preserving preferred grapheme column.
- k: previous logical line while preserving preferred grapheme column.
- w: beginning of the next lexical run.
- b: beginning of current or previous lexical run.
- e: end of current or next lexical run.
- 0: first column of the logical line.
- dollar: end of the logical line.
- gg: first line.
- count gg: one-based target line.
- G: last line.
- count G: one-based target line.

Use three predictable lexical classes:

- whitespace;
- Unicode alphanumeric plus underscore;
- punctuation/other.

Arrow keys may alias motions while composer Vim is active.

Normative boundaries:

- A logical line excludes its newline separator. dollar lands on the final grapheme of a nonempty line, never on the newline or insertion boundary. On an empty line it remains at that line's stable empty Normal position.
- w, b, and e cross logical newlines as whitespace boundaries and then reach the next/previous meaningful lexical run. At document bounds they clamp/no-op.
- An empty document has one virtual empty logical line and a Normal cursor at byte boundary zero. A Normal cursor at end-of-line uses the last grapheme when one exists; the empty-line block is a visual insertion-boundary sentinel, not an invalid byte index.
- A motion returns a semantic endpoint plus characterwise/linewise and inclusive/exclusive metadata. Operators consume that metadata; do not reconstruct ranges from cursor arithmetic.

### 17.4 Insert commands

- i: insert before cursor.
- a: insert after cursor.
- I: first non-whitespace insertion point.
- A: logical line end.
- o: create a line below and enter Insert.
- O: create a line above and enter Insert.
- Escape: complete the insertion/change transaction and return to composer Normal.

o/O should copy the current line's leading indentation.

Maple-specific Enter behavior:

- Plain Enter sends the current draft in Insert and Normal, preserving existing chat ergonomics.
- Shift-Enter inserts a newline only in Insert.
- o/O provide Vim-native newline creation without sending.

### 17.5 Operators and counts

Required operators:

- d delete;
- c change and enter Insert;
- y yank to the unnamed register.

They compose with every milestone motion.

Repeated operators are linewise:

- dd;
- cc;
- yy.

Rules:

- Operator and motion counts multiply. 2d3w affects six word motions.
- e and dollar targets are inclusive.
- w targets are exclusive.
- cw special-cases a non-whitespace starting position to the equivalent of ce, matching ordinary Vim expectations rather than consuming following whitespace. On whitespace it follows the normal c plus w target.
- j, k, gg, and G targets are linewise.
- c starts one insertion transaction after deletion.
- cc leaves one editable line and should retain leading indentation.
- dd handles first line, last line, and final newline correctly.
- yy produces a linewise register.

Other required changes:

- x deletes count graphemes into the unnamed register.
- p/P paste after/before for characterwise content.
- p/P paste below/above for linewise content.
- Counts apply to x, p, P, motions, operators, and dot.
- u undoes count complete Vim changes.
- Ctrl-R redoes count complete changes.

The unnamed register is required even though full named-register compatibility is deferred. Secondary-C/Secondary-V remain system clipboard operations where the selected profile binds them.

Linewise behavior is normative:

- A linewise range includes complete addressed logical lines and their separator when one exists.
- Deleting/yanking a nonfinal line captures its trailing newline. Addressing the final line of a multi-line document without a final newline uses the preceding separator so no orphan blank line remains.
- dd on the only line leaves an empty document. cc on any line leaves one editable line at that location and preserves that line's leading indentation; it enters Insert at the first non-whitespace insertion point.
- After linewise delete, the cursor lands on the first grapheme of the next surviving line, or the previous final line if there is no next line, with the empty-line sentinel rule above.
- A linewise p inserts complete lines below and P above; the cursor lands on the first non-whitespace grapheme of the first pasted line. Behavior is identical whether the document originally ended in a newline.
- dw at the final word deletes to the end of that word. Immediately before a newline, it does not unexpectedly join lines unless the computed w motion actually crosses into the next lexical run; tests pin both cases.

### 17.6 Visual

- v enters or toggles characterwise Visual.
- Milestone motions extend the selection.
- d, x, c, and y act immediately.
- p/P replace the selection from the register.
- Escape exits Visual without changing text.
- iw/aw work as Visual targets as well as operator targets.

Entering v selects the current grapheme immediately; on an empty document it selects the stable empty sentinel without creating an invalid range. Anchor and head are inclusive grapheme positions, so reverse selections include both endpoints when converted to TextInput's end-exclusive range. After Visual y, collapse to Normal on the former selection start. After d/x, use the deletion cursor rule. After c, enter Insert at the deletion start in the same change transaction.

Visual p/P replaces the selection with the pre-command unnamed register and then stores the displaced selected text in the unnamed register, matching Vim's swap-like register behavior. The placement distinction does not move insertion outside the selected range for characterwise Visual replacement. Add single-grapheme, reverse-direction, combining-mark, and emoji tests.

Visual Line, Visual Block, multiple cursors, and named registers are deferred.

### 17.7 Text objects

iw:

- selects the current lexical run;
- on whitespace, selects the next meaningful run when possible;
- uses semantic boundaries, not a remembered byte range.

aw:

- selects iw plus trailing whitespace;
- when no trailing whitespace exists, includes leading whitespace;
- preserves predictable behavior at line/document boundaries.

This semantic representation is necessary so ciw can repeat on a differently sized word.

### 17.8 Dot repeat

Dot is required and must repeat the last complete mutating semantic command.

It must not replay raw key events.

Required examples:

~~~text
ciwhello<Escape>  then dot  changes the next current word to hello
A!<Escape>        then dot  appends ! at another logical line
3x                then dot  deletes three graphemes again
ohello<Escape>    then dot  opens another line containing hello
~~~

Use structured recipes such as:

~~~rust
pub enum RepeatRecipe {
    Insert {
        entry: InsertEntry,
        inserted: InsertDelta,
    },
    Operator {
        operator: Operator,
        target: MotionOrTextObject,
        count: usize,
        inserted: Option<InsertDelta>,
    },
    DeleteChars {
        count: usize,
    },
    Paste {
        placement: PastePlacement,
        register: RegisterSnapshot,
        count: usize,
    },
    VisualChange {
        operator: VisualOperator,
        grapheme_span: usize,
        register: Option<RegisterSnapshot>,
        inserted: Option<InsertDelta>,
    },
    OpenLine {
        placement: OpenLinePlacement,
        indentation: IndentationPolicy,
        inserted: InsertDelta,
    },
}
~~~

Use exactly one recipe for each change. o/O use OpenLine, not Insert, so there is no overlapping representation. Store semantic targets such as InnerWord, not original offsets. Store the committed insertion delta, not key events, so IME input, keyboard layout, and custom bindings do not alter repeat behavior.

Visual d/c/p also update last_change. Normalize the inclusive selection to a grapheme_span independent of original direction/offset. Repeat applies the same operator to that many graphemes beginning at the new Normal cursor, using the saved pre-command register for paste and the committed InsertDelta for change. If fewer graphemes remain, clamp according to the ordinary deletion/change boundary rule; zero-span/empty-sentinel mutations that change nothing do not replace last_change.

InsertDelta is a normalized edit script relative to the insertion entry point, not merely the final inserted string. It records ordered committed text replacements/deletions and cursor-relative edits produced during the transaction, including Backspace/Delete, selection replacement, cursor movement followed by edit, and IME committed replacement. It does not record raw physical keys or uncommitted composition. Replaying applies the normalized edits to the new semantic entry point; if a required local precondition cannot be satisfied, the repeat fails harmlessly without corrupting text.

Additional rules:

- count dot runs the saved recipe that many times without replacing it;
- motions, yanks, mode transitions, undo, and redo do not replace last_change;
- a no-op change does not become last_change;
- one dot command, including a count such as 2., is one undo unit for the entire counted replay;
- a failed repeat leaves last_change intact and reports a harmless status.

### 17.9 Undo transactions

The current TextInput snapshot/typing-run history is not sufficient for Vim composition. ciwhello Escape must undo as one change, not a deletion plus multiple insertion records.

Add an explicit edit transaction:

1. Begin on i/a/I/A/o/O/c.
2. Lazily record the pre-change snapshot at first mutation.
3. Suppress nested history entries while Insert continues.
4. Include the operator deletion and insertion in one transaction.
5. Commit on Escape.
6. If nothing changed, add no history entry.
7. Clear redo on a new committed edit.
8. Treat dot replay as one nested-disabled transaction.

set_text, send/clear, disabling Vim, task replacement, and external draft restoration must deliberately terminate/reset transient Vim state.

Use this deterministic termination contract:

| Event during Insert/change | History | last_change | register | resulting Vim state |
|---|---|---|---|---|
| Escape | commit one transaction if mutated | update from committed semantic recipe | preserve/update only if operator changed it | Normal |
| Send or clear after successful send | commit draft edit for local history, then clear/reset task draft history | update only from the completed edit; do not make send a dot recipe | preserve | Normal on the new empty draft |
| composer.set_text or external draft restore | close current transaction without synthesizing a repeat recipe, replace text, clear undo/redo tied to the old draft | clear | preserve only if its contents are independent text; otherwise clear deliberately | Normal with clamped cursor |
| Task/screen switch | commit a real local edit into that task's draft, cancel all pending grammar, store insertion revision | keep that task's committed recipe only while its draft revision remains compatible | retain an unnamed register per task/draft; never expose it in another task | inactive until restored |
| Switch to Standard/disable Vim | commit a real edit, cancel grammar/Visual, collapse safely | retain only for restoration if the same draft revision returns; otherwise clear | preserve for same draft only | Disabled |
| Mouse click/caret move in composer | commit current insertion transaction if mutated, cancel operator/count/text-object/Visual, translate to the clicked grapheme or empty sentinel | retain the just-committed recipe | preserve the current task's register | Normal; never half-pending |

Queue draft edit/restore follows the external-draft rule and carries a draft revision. No transient byte offset, undo entry, or dot recipe may be applied to a different task or incompatible draft revision.

Mouse policy is intentionally Vim-like for this milestone: a direct click inside the composer places the block cursor and enters Normal, regardless of the previous editor mode. A click outside commits any active insertion transaction, leaves the editor in inactive Normal, pushes/reconciles the destination application region, and moves GPUI focus off TextInput. Double/triple-click rich selection behavior is deferred rather than leaving an ambiguous Visual/Insert state.

### 17.10 Key interception and existing composer behavior

Typed Vim command handlers run:

1. after a truly modal popup has consumed its keys;
2. before ordinary TextInput actions and platform text insertion.

In Normal and Visual:

- consume unknown unmodified printable characters;
- do not allow them to fall into GPUI text insertion;
- make arrow/Home/End/Backspace/Delete action handlers mode-aware.

The raw-key hook is only this unmatched-printable/IME safety guard. It must not hard-code command bindings or count digits. Remapping composer.vim.motion or composer.vim.begin_operator in keymap.json must change actual behavior, remove the old effective binding, update conflicts/which-key, and leave the pure engine unaware of the physical key.

The existing composer on_key hook handles slash-palette movement and prompt-history Up/Down. Gate it:

- Insert: current slash/history behavior remains.
- Normal/Visual: Vim owns printable commands and motions.
- Focused menus/questions/permissions: their modal context shadows composer Vim.

The current Chat type-to-compose behavior must be disabled in application Vim Normal or it will turn j/k/g/leader into draft text. Preserve it in Standard.

Escape precedence:

- marked IME composition resolves/cancels safely first;
- Insert Escape enters composer Normal and stops propagation;
- Visual/operator Escape cancels to composer Normal;
- composer Normal Escape enters application Normal/restores prior region;
- only an unconsumed Chat-level Escape may reach existing menu-close/run-stop behavior.

### 17.11 Mode UI

Render a compact composer badge:

- NORMAL;
- INSERT;
- VISUAL;
- optionally NORMAL · 3d while a count/operator is pending.

Do not show it in Standard.

Cursor:

- Insert: current thin caret.
- Normal: block covering the current grapheme.
- Visual: selection highlight plus a distinct head.
- Empty/end-of-line Normal: stable minimum-width block.

Mode-only changes must notify/render even when draft text did not change.

### 17.12 Composer/application transition

- Vim profile composer starts in Normal.
- i/a/I/A/o/O enter Insert.
- Insert Escape returns to composer Normal.
- Ctrl-W region motion or ga can leave composer Normal for application navigation.
- Composer Normal Escape returns to the previous semantic region.
- Application gi focuses the composer and enters Insert at the saved insertion point.

This gives both real composer Normal mode and the fast streamed-response navigation that motivated the feature.

### 17.12.1 Focus and routing table

The following is normative. "Focus owner" means the GPUI element that receives the next dispatch, not merely a semantic highlight.

| Current state/input | Consumer | Focus owner after | Semantic region/mode after | Propagation |
|---|---|---|---|---|
| Composer Insert, Escape | composer editor | TextInput | composer, editor Normal | stop |
| Composer Visual or pending operator/count, Escape | composer editor | TextInput | composer, editor Normal with pending state cleared | stop |
| Composer Normal, Escape | application transition action | prior region focus proxy, Transcript fallback | popped prior region, application Normal | stop after focus moved off TextInput |
| Application region, gi | application action | TextInput | composer Insert at revision-checked last insertion | stop |
| Composer Normal, ga | transcript action | transcript/application focus proxy | Transcript at newest assistant | stop |
| Composer Normal, Ctrl-W h/j/k/l | region action | destination region focus proxy | destination application region | stop |
| Task/screen switch | root semantic controller | new screen's region focus proxy, or TextInput only if explicitly restored | reconciled selection; transient editor grammar cancelled | stop |
| Popup/dialog opens | popup/dialog | popup/dialog focus | underlying app/editor state suspended | shadow underlying bindings |
| Popup/dialog closes | popup/dialog then return stack | exact validated prior focus proxy | prior region/editor state, with stale targets reconciled | stop |

Before leaving composer Normal, move GPUI focus off TextInput synchronously so the next application key cannot re-enter the editor. Overlays shadow both application and composer maps. At application root, Escape reaches the existing Chat close/run-stop behavior only when no overlay, dialog, pending Vim grammar, region-return transition, or editor transition consumed it.

Key ownership while composer Normal is focused:

- local editor: h/j/k/l, w/b/e, 0/dollar, gg/G, operators, text objects, counts, v, x, p/P, u/Ctrl-R, dot, i/a/I/A/o/O;
- reserved application: ga, gi, bracket a/d, Ctrl-W h/j/k/l, colon, slash, Space leader;
- colon opens the root action palette; slash opens the current application's search action, using Transcript/task search as the Chat fallback. It is not a composer Vim text-search grammar in this milestone and never inserts slash in Normal;
- Enter sends the draft; Shift-Enter is unavailable in Normal/Visual and must not insert or send;
- Visual Enter is unavailable and consumed;
- popup/dialog bindings win over both groups.

The effective keymap, not a second parser, implements this precedence. A shorter prefix and longer complete chord use GPUI's pending-input timeout and which-key display.

### 17.13 Composer tests

Pure-engine tests must cover:

- ASCII, combining characters, emoji, punctuation, whitespace, empty and multiline text;
- preferred column across uneven lines;
- 0 versus count parsing, including 10j;
- gg/G with and without counts;
- motion inclusivity/exclusivity;
- cw special case, word motions across newlines, empty documents, and final-newline operations;
- 2d3w;
- dd/cc/yy at first/last lines and trailing-newline boundaries;
- iw/aw on words, punctuation, and whitespace;
- characterwise/linewise register and p/P;
- forward/reverse Visual selections;
- single-grapheme/reversed Unicode Visual selection and Visual paste register replacement;
- undo/redo transaction boundaries;
- dot for insert, append, open line, ciw, delete, paste, Visual d/c/p, insertion Backspace/replacement/IME commit, and counted repeat including one-unit 2.;
- no-op commands not affecting history or dot.

GPUI integration tests must prove:

- Standard TextInput behavior is unchanged.
- Only the composer opts in.
- Search, login, rename, settings, question, and MCP inputs keep ordinary editing while the Vim profile is active.
- Normal printable commands do not insert.
- Insert still uses EntityInputHandler and IME.
- Escape precedence is correct.
- Enter/Shift-Enter behavior matches the contract.
- Slash palette and prompt history still work.
- Live profile switching safely resets state.
- Mode badge and cursor follow state.
- One u undoes all of ciwhello Escape.
- Remapping a composer motion/operator changes the real command, disables the old binding, and updates Shortcut Settings/which-key.
- Composer Normal j moves the caret while Transcript j moves semantic selection.
- ga, gi, colon, slash, leader, bracket commands, and Ctrl-W route according to the table from composer Normal.
- Rapid diw, 10j, d2d, and pending Escape work with no intervening render/context refresh.
- Invalid editor grammar never inserts, sends, or reaches Chat; Escape/focus/profile changes cancel it, while editor grammar has no time timeout.
- Static GPUI chord timeout/cancellation and shorter-binding dispatch preserve/clear key provenance correctly.
- Composer Escape cannot reach Chat run-stop before the documented final application-root stage.
- Profile switching mid-Insert, delayed old-profile prefix dispatch, mouse-to-Normal, and external draft replacement follow the termination table.

## 18. Python Code Mode

### 18.1 Settings and default state

Expose two controls backed by one normalized runtime authority tuple:

~~~rust
pub enum PythonCodeMode {
    Off,
    DeveloperPreview,
}

pub enum UiControllerAccess {
    Off,
    ReadOnly,
    FullAccess,
}

pub struct ProgrammabilityAuthority {
    pub code_mode: PythonCodeMode,
    pub controller: UiControllerAccess,
}
~~~

Defaults:

- Python Code Mode: Off.
- Maple UI Controller: Off.

The actual authority tuple is session-scoped in this Developer Preview and starts Off/Off on every process launch, account change, or sign-out. Persist the one-time disclosure acceptance and interpreter preference, not active Code Mode or controller authority. This fail-closed choice prevents a failed downgrade write or stale file from silently restoring Python/Full Access after a crash. A future decision to persist authority requires a separate reviewed revocation/startup design.

Normalize every transition atomically: code_mode=Off implies controller=Off, and ReadOnly/FullAccess is invalid unless code_mode=DeveloperPreview. The controller selector is disabled while Code Mode is Off. Turning Code Mode Off cancels active Python executions, revokes controller leases, and stops all task kernels.

### 18.2 Disclosure

Enabling Developer Preview requires a one-time direct-user disclosure in the UI.

Required language:

> Developer Preview: Python runs with your macOS user permissions. Maple provides project/scratch paths, no supported network API, and best-effort Python guardrails, but this is not a hardened sandbox. Python code may access other files, processes, credentials available to your user, or the network. Enable it only for tasks you trust.

The status UI should say:

~~~text
Project files: intended read/write
Scratch: read/write
Network: no supported Maple API; native Python may still connect
Process isolation: separate worker; not an OS security boundary
~~~

Do not say merely Sandboxed or Network disabled.

Python's own documentation explicitly warns that Python-level audit hooks are not a sandbox. That warning should inform both implementation and copy: https://docs.python.org/3/library/sys.html#sys.addaudithook

### 18.3 Kernel identity

Use:

~~~rust
pub struct KernelKey {
    pub account_scope: String,
    pub task_id: String,
}
~~~

The task ID is the durable Goose session ID.

Each kernel records:

- canonical project root loaded from durable task metadata;
- private task scratch path;
- kernel generation;
- interpreter path/version/engine;
- active execution ID;
- last-used time;
- controller policy epoch.

Before every execution, verify that:

- the task still exists;
- it still belongs to the account;
- its canonical project root is valid and unchanged.

If not, stop the kernel and return a structured error. Never fall back to the selected UI project, HOME, or another task's root.

Every caller supplies only account/task identity. The backend reloads durable
task metadata and derives the canonical root rather than trusting a path copied
from whichever task the UI currently shows. Derive the scratch directory from
an account-scope digest plus a sanitized/hashed task ID, create it with
owner-only permissions, and validate the exact resolved target before deletion.

### 18.4 Lifecycle

- Lazy-create on the first Python call for a task.
- Persist namespace while the app runs, even when another task is selected.
- Serialize to one active execution per kernel.
- Return busy rather than building an unbounded queue.
- Interrupt attempts to stop the current execution while retaining state if safe.
- Reset clears the user namespace and pending async tasks in the same worker.
- Restart terminates the process group/job, reaps the direct worker, performs best-effort cleanup of tracked descendants, and launches a fresh interpreter.
- Hard timeout, protocol corruption, or failed interrupt kills and marks/restarts the worker.
- Task deletion first tombstones the kernel key, revokes admission/leases, stops and reaps the direct worker plus tracked descendants, and only then removes the exact validated scratch directory.
- Task archive retains the kernel.
- Logout kills/reaps all direct workers for that account and best-effort tracked descendants.
- App quit stops admission, cancels executions, terminates each process group/job, reaps each direct worker, and verifies tracked/cooperative descendants exit where observable.

Cap active kernels, initially eight. Eviction must be explicit in result/UI because it loses in-memory state. Prefer least-recently-used idle eviction; never evict a running kernel. If all eight are running, a ninth request returns a structured capacity_exceeded immediately rather than waiting indefinitely. Clear Scratch is allowed only after its kernel is idle/stopped; otherwise stop first or return busy.

Use actor ownership so service/global locks are not held while waiting on Python or GPUI.

### 18.5 Interpreter resolution

The Developer Preview may use an external native interpreter, but resolution must be deterministic and visible:

1. Explicit `MAPLE_CODE_MODE_PYTHON` path.
2. The saved explicit interpreter preference.
3. A future bundled interpreter if present.
4. Resolved `python3` from the login/current PATH.
5. Common platform paths as a final developer fallback.

Requirements:

- Resolve an executable path, not an arbitrary shell command.
- Require Python 3.10 or newer and probe version/engine dependencies before accepting.
- Select IPython only when it imports successfully in the isolated probe;
  otherwise use the CPython engine.
- Probe a changed preference before publishing it. On success, atomically swap
  the preference and stop existing kernels; an invalid explicit path fails
  closed instead of silently falling through.
- Show path, Python version, and IPython/CPython engine in Settings.
- Refuse an unusable interpreter with a clear error.
- The Nix dev shell provides a pinned nixpkgs Python plus IPython so validation
  is reproducible and does not rely on Apple's system Python.

Bundling Python in every release artifact and environment/package management are explicit follow-ups.

### 18.6 Worker process boundary

Python runs outside the GPUI process.

Use private inherited pipes, not a loopback TCP server:

- host-to-worker framed requests on a dedicated inherited descriptor;
- worker-to-host framed protocol on a second dedicated inherited descriptor;
- ordinary child stdout and stderr on separate capture pipes;
- user-code stdin replaced with a controlled EOF/error stream rather than either protocol descriptor.

This keeps input(), sys.stdin.buffer.read(), os.read(0, ...), user print, native output, and subprocess output from stealing or corrupting protocol frames. If dedicated bidirectional descriptors are temporarily impossible on one platform, fail or use a clearly isolated equivalent; do not quietly place host messages on user stdin or mix arbitrary native stdout with JSON framing.

After bootstrap handoff, mark protocol descriptors close-on-exec/non-inheritable so ordinary subprocesses do not receive them; preserve only stdout/stderr capture inheritance where required. The fixed worker retains the descriptors privately, while user code still runs in the same native process and therefore is not treated as adversarially isolated.

The worker has a concurrent control architecture:

- one dedicated protocol-reader thread continuously reads host frames during execution;
- Python/IPython user execution remains on the Python main thread;
- sdk_result delivery is posted thread-safely into the active asyncio loop/future;
- controller_changed, cancellation, and shutdown update atomic/thread-safe control state immediately;
- one serialized protocol writer owns every worker-to-host frame;
- a second execute request while one is active receives busy and is never run concurrently.

Cooperative Python cancellation can be scheduled through the loop. When Python or native code prevents it, the Rust host performs the platform-specific interrupt/termination sequence; protocol-reader liveness alone is not mistaken for execution cancellation.

Protocol:

- fixed protocol version;
- four-byte big-endian frame length;
- UTF-8 JSON payload;
- maximum frame size;
- kernel generation;
- reject unknown, oversized, duplicate-terminal, stale-generation, and out-of-order messages.

Use a common envelope with version, kind, kernel_generation, and a message sequence. Correlation fields are kind-specific rather than fake IDs on connection messages:

- hello/ready carry a random handshake_nonce and generation, no program/execution;
- execute carries request_id, program_id, execution_id, and optional model_run_id;
- cancel carries request_id plus the exact program_id/execution_id;
- reset/restart/shutdown carry a control request_id and generation;
- controller_changed carries policy_epoch and generation;
- sdk_call/sdk_result carry sdk_request_id plus program_id/execution_id;
- display/completed/fatal carry program_id/execution_id when execution-scoped, otherwise a control request or fatal generation.

Validate an explicit state machine: Spawning to Handshaking to Idle to Running(program, execution) to Interrupting to Idle/Dead. Only controller_changed, matching sdk_result, matching cancel, and shutdown are accepted concurrently with Running. Duplicate/out-of-order terminal messages, generation mismatches, an SDK result for a nonpending request, or execution output while Idle are protocol corruption and force restart.

Host-to-worker:

- hello;
- execute;
- reset;
- restart;
- cancel;
- shutdown;
- controller_changed;
- sdk_result.

Worker-to-host:

- ready;
- display;
- sdk_call;
- completed;
- fatal.

Raw stdout and stderr capture pipes are the sole source of ordinary output events, including os.write, native libraries, and subprocesses. Attribute their bytes to the active kernel generation/program/execution. A host-side aggregator assigns one merge sequence to stdout, stderr, protocol display, and terminal arrivals as their respective pumps deliver them; cross-descriptor ordering is best-effort and must be labeled as such. At terminal completion the worker flushes Python streams and writes an execution-specific high-entropy barrier to each raw pipe before emitting completed. The host strips the barriers and does not surface terminal completion until both capture pumps have observed them or a bounded drain timeout forces restart. Output after a barrier indicates a lingering/background producer, is marked contaminated, and forces a restart rather than being attributed to a later cell.

Use a fixed checked-in bootstrap script. Do not generate Python source ad hoc from Rust.

The protocol channel is private by process construction, but native Python is not a security boundary. A hostile program can enumerate or corrupt inherited descriptors; treat that as protocol failure and restart, not as a sandbox escape that Maple claims to prevent.

### 18.7 Execution engine

Prefer IPython InteractiveShell.run_cell_async when the selected interpreter has IPython. Official API reference: https://ipython.readthedocs.io/en/stable/api/generated/IPython.core.interactiveshell.html

Provide a CPython fallback:

- one persistent globals namespace;
- compile with ast.PyCF_ALLOW_TOP_LEVEL_AWAIT;
- a persistent event-loop strategy;
- capture the final expression;
- retain imports and variables between calls.

Example:

~~~python
x = 41
x + 1
~~~

returns 42 and leaves x available for the next cell.

Capture:

- interleaved stdout/stderr events;
- final text representation;
- structured exception type, message, and traceback;
- bounded text/plain rich-display representation where feasible;
- engine/version;
- duration;
- truncation;
- whether forced restart occurred.

At execution completion, cancel unowned pending async tasks. Also detect Python-managed threads and tracked/cooperative process-group descendants created by the cell. Code Mode does not promise persistent background work in this milestone: give observable work a short cooperative grace period, then mark the kernel contaminated and force Restart if it remains or writes after the output barrier. A native extension can create unenumerable threads or a subprocess can setsid/double-fork out of the group; cleanup is best-effort under the same non-sandbox Developer Preview disclosure. A soft interrupt may leave partially mutated Python globals; show that fact and offer/perform Restart according to the result. Persistent variables are useful; invisible background jobs are not.

### 18.8 Bounds

Initial defaults:

| Resource | Limit |
|---|---:|
| Source per execution | 128 KiB |
| Protocol frame | 1 MiB |
| Captured output total | 256 KiB |
| Single result/display item | 64 KiB |
| Display item count | 16 |
| Default execution time | 60 seconds |
| SDK calls per execution | 256 |
| Interrupt grace | 500 to 750 ms |
| Active kernels | 8 |

Make limits constants/config with tests.

An output flood cancels the execution and terminates/restarts when necessary. It does not continue producing discarded data indefinitely.

### 18.9 Cancellation and Stop

Every execution has an always-present host-generated `program_id` and
`execution_id`. A model-triggered execution additionally has the owning
`model_run_id` and Goose `CancellationToken`. The protocol retains a UserCode
origin for trusted compatibility paths, but this preview does not mount a
human execution surface in chat.

Stopping:

1. Revoke the execution's UI capability lease.
2. Mark the execution cancelled so late sdk_call frames are rejected.
3. Request a cooperative loop/interpreter interrupt, then use the platform-appropriate hard-interrupt mechanism if needed. On Unix this may include a process-group signal; do not assume Windows has an equivalent SIGINT path.
4. Wait the short grace period.
5. Terminate the process group/job if it does not stop, reap the direct worker, and verify tracked/cooperative descendants exited where observable; report the native containment limit honestly.
6. Restart or mark the kernel stopped, and report what happened.

Once cancelled, an execution ID can never invoke another action. A
model-triggered Stop revokes the execution lease and cancels the owning Goose
run as well, so that same run cannot call `python_code` again or invoke another
controller action. Track stopped model run IDs at the host boundary until the
run is terminal.

While any Python/model UI program runs, or a failed teardown retains an exact
cleanup identity, its Stop/Retry Stop control remains visible in Settings >
Programmability. There is intentionally no global program HUD.

Cancellation does not undo completed actions. Audit/result copy must say so.

### 18.10 Intended filesystem/network posture

Launch with:

- cwd set to the task's canonical project root;
- HOME and temp paths pointed at private task scratch;
- Command::env_clear followed by a minimal explicit allowlist needed for locale, interpreter execution, project/scratch metadata, and platform runtime behavior;
- no intentionally passed Maple/OpenSecret auth tokens, credential paths, proxy variables, dynamic-loader injection variables, cloud credentials, or unrelated parent environment;
- no database handles;
- explicit MAPLE_PROJECT_ROOT and MAPLE_SCRATCH_DIR metadata.

The ordinary intended capability is:

- project root: read/write;
- task scratch: read/write;
- network: no supported Maple API and best-effort ordinary Python guardrails; native Python still has the macOS user's ability to connect;
- outside-project files and subprocesses: discouraged/guarded through ordinary Python APIs where practical, but still reachable by native Python under the user's OS permissions.

The worker may install best-effort import/audit guards to prevent ordinary socket, subprocess, outside-write, or credential access. These are optional ergonomics and useful guardrails, not the authority boundary. Native modules, ctypes, direct syscalls, or reading the user's existing app/auth files may bypass them. "No Maple auth tokens" means Maple does not pass tokens into the child; it does not claim the native worker cannot discover files its macOS user can read. Tests and UI must reflect the honest Developer Preview boundary, and one failed socket call is never evidence of a sandbox.

Production hardening is a separate sandboxed macOS helper/XPC design with no network entitlement and explicit file grants, plus equivalent Linux/Windows containment.

## 19. User-visible Programmability controls

The Developer Preview's human-facing surface is Settings-only. Python
execution remains model-driven through `python_code`.

### 19.1 Entry point

- Settings > Programmability is the only visible Code Mode/controller surface.
- `code_mode.open` remains a stable compatibility action and navigates there.
- Code Mode actions are excluded from command-palette discovery and the
  default Standard/Vim keymaps.
- No chat strip, expandable human REPL, or global program HUD is mounted.

### 19.2 Surface

Settings > Programmability contains:

- Developer Preview disclosure and explicit session-only Off-by-default copy;
- Python Code Mode Off/Developer Preview and Maple UI Controller
  Off/Read Only/Full Access;
- interpreter preference plus resolved path, Python version, and
  IPython/CPython engine;
- limits and current enforcement summary;
- active account/task kernels, source task, state, current or most recent
  action, elapsed time, and generation;
- retained model-program and failed-teardown records;
- Stop/Retry Stop, Reset, Restart, and Clear Scratch lifecycle controls;
- bounded execution history and action audit with actor/transport labels.

The surface discloses that all model `python_code` calls for the same
account/task share one process-local namespace. Variables, imported modules,
in-memory data, and secrets created by one call may be read or changed by a
later call. Reset/Restart visibly destroy that shared state and are audited.
Clear Scratch additionally follows the preconditions in Section 18.4.

### 19.3 Authority

Code Mode and controller-mode changes are direct-human-only. Settings reflects
the normalized runtime tuple and cannot restore active authority from disk.

## 20. Model-facing python_code tool

Add python_code to MapleDeveloperClient only when Developer Preview is currently enabled.

Suggested discriminated input:

~~~json
{
  "operation": "execute",
  "code": "x = 41\nx + 1",
  "timeout_seconds": 30
}
~~~

Other operations:

~~~json
{ "operation": "status" }
{ "operation": "reset" }
{ "operation": "restart" }
~~~

General Python works with the controller Off.

Result includes:

- kernel generation;
- program ID;
- execution ID;
- interpreter/engine;
- status;
- stdout;
- stderr;
- result;
- structured error;
- duration;
- truncation;
- cancelled/timed-out;
- forced restart.

The tool is absent from list_tools when Code Mode is Off. A stale already-described invocation rechecks current policy and fails closed.

execute, reset, and restart act on the source task's shared model-call kernel.
Sequential `python_code` calls share its namespace; reset/restart therefore
visibly destroy shared in-memory state. The tool description says so and
audit/history attributes the operation to the model. This is not protected by
UI Controller mode because general Python itself is allowed with the
controller Off. Clear Scratch is not a model tool operation in the first
milestone; it remains an explicit Settings action with the idle/stopped and
exact-target checks in Section 18.4.

### 20.1 Runtime bridge

Recommended ownership:

~~~text
GPUI main thread
  MapleApp semantic dispatcher
  UiController request receiver
             ^
             | bounded mpsc + oneshot
             v
AgentBackend private Tokio runtime
  CodeModeService
             ^
             | framed process protocol
             v
per-task Python worker
~~~

Do not route request/response controller calls through AgentServiceEvent, which is a cloneable one-way event stream. Use a separate bounded channel carrying:

- request ID;
- host-assigned source task/run/execution;
- ActionCall or semantic query;
- oneshot response;
- cancellation/policy epoch.

The GPUI root owns a foreground-executor receiver task/notifier that awaits channel readiness and explicitly wakes/schedules a cx.update on the GPUI event loop. Do not depend on an incidental render frame or input event. Each wake drains a bounded batch, then yields and reschedules itself if work remains so rendering/input stays responsive. Dropping MapleApp/window cancels the receiver and closes pending oneshots.

Initial bridge contract:

- bounded queue depth: 64 requests;
- UI drain batch: at most 16 requests per frame/tick, then yield for rendering/input;
- enqueue timeout: 250 ms, returning controller_busy/queue_full;
- ordinary response timeout: 30 seconds; events.wait has its own explicit maximum of 60 seconds plus a small broker drain margin;
- app unavailable/quitting, dropped receiver/oneshot, and closed window return structured ui_unavailable/cancelled errors;
- cancellation removes or tombstones queued work and is checked again on the UI thread;
- no kernel/service/global lock is held while enqueueing or awaiting a UI response.

Async actions return accepted plus an operation ID and register an ActionExecutionHandle in a bounded operation table. That handle owns cancellation/backend task state and emits exactly one terminal event/result. Recheck policy immediately before submitting an irreversible backend effect. Before submission cancellation can prevent it; after submission it may complete and audit completed_after_cancel_request.

A ProgramRecord owns the root Python execution plus every operation handle it started. When the cell returns accepted, the execution can become terminal and the kernel can accept a later cell, but the ProgramRecord, its visible Stop/audit state, policy lease, and bounded operation budget remain until all handles reach terminal status. A terminal Python execution cannot originate new SDK calls; its already-created handles may only finish/cancel under current policy. Multiple surviving ProgramRecords are shown separately or by a Stop All control and count against the global operation bound.

Stop or controller downgrade after cell completion still cancels/revokes the surviving handles. Reset/Restart first Stop every nonterminal ProgramRecord for that task, wait the bounded grace, and then reset/replace the worker; already-submitted irreversible effects may still finish and must be audited. A new execution gets a new ProgramRecord and cannot borrow a prior record's lease or action budget.

### 20.2 maple-agent seam

MapleAgentHostResources should receive an optional trait object or broker handle defined below app, such as PythonCodeModeHost. MapleDeveloperClient uses it for python_code without depending on GPUI.

The current developer-tool call context has a source session and CancellationToken but no stable owning model-run identity. Add a Maple-owned run-control seam rather than pretending the token is an ID:

~~~rust
pub struct MapleModelRunContext {
    pub run_id: ModelRunId,
    pub task_id: String,
    pub cancellation: CancellationToken,
    pub control: Arc<dyn ModelRunControl>,
}

pub trait ModelRunControl {
    fn cancel_run(&self, run_id: &ModelRunId) -> Result<(), RunControlError>;
    fn is_terminal(&self, run_id: &ModelRunId) -> bool;
}
~~~

Create/register the run ID where Maple starts a model generation, carry the context through the agent/developer-tool host resources into call_tool, and remove/tombstone it only when the run is terminal. Stop calls cancel_run and the token; terminal cleanup removes the registry entry. A tool invocation cannot mint or choose this ID.

Forward the source session ID and run context unchanged as authoritative identity/cancellation context. Treat the supplied working directory only as a consistency hint: the Code Mode backend independently reloads durable project metadata and derives the canonical root for every human and model execution.

## 21. maple_gpui SDK

### 21.1 Availability

The fixed maple_gpui package is installed in every Maple worker so code has one stable import contract. Rust enables its controller transport only when access is Read Only or Full Access. When controller is Off:

- general Python still works;
- importing the same full `maple_gpui` SDK succeeds, while every host call
  raises a structured `ControllerDisabled` error;
- the model-facing controller skill is not advertised.

An old module reference after downgrade cannot retain power because Rust rechecks every call.

### 21.2 Generic API

Required initial API:

~~~python
import maple_gpui as maple

status = await maple.ui.status()
tasks = await maple.tasks.list(limit=100)
target = next((task for task in tasks if not task.active), None)
assert target is not None, "create a second harmless task first"

catalog = await maple.actions.list(
    query="task",
    available_only=False,
)
descriptor = await maple.actions.describe("task.open")

result = await maple.actions.invoke_and_wait(
    "task.open",
    {"task_id": target.id},
    precondition=target.precondition,
    timeout=10,
)
~~~

Then add thin generated/domain conveniences:

~~~python
tasks = await maple.tasks.list()
await maple.workspace.open_task(tasks[0].id)
await maple.transcript.focus_next(kind="annotation")
~~~

The generic discovery/invoke API is authoritative. Action listings are compact
and expose `needs_arguments`; use `actions.describe` to fetch full argument
schemas. Prefer `invoke_and_wait` when completion matters because it captures
the event cursor before invocation and cannot miss a fast completion. Plain
`invoke` plus `events.wait` remains available for workflows that deliberately
manage their own cursor. Convenience wrappers delegate to this API.

### 21.3 SDK transport

SDK calls are sdk_call frames on the worker's existing private protocol.

Python sends:

- method;
- JSON arguments;
- optional target/action precondition;
- its local request ID.

It does not send a trusted actor, transport, controller mode, policy epoch, account, or capability token.

Rust attaches:

- account scope;
- source task;
- model run ID when applicable;
- host-generated program ID;
- kernel generation;
- execution ID;
- invocation actor (Model for model-triggered code; UserCode only for a trusted
  compatibility path) and transport Python;
- current controller policy/epoch;
- cancellation.

### 21.4 Required capabilities

maple.ui:

- describe;
- status;
- query;
- reveal;
- current_selection.

maple.actions:

- list;
- describe;
- availability;
- invoke.
- invoke_and_wait.

maple.events:

- wait.

Thin namespaces:

- tasks;
- projects;
- workspace;
- transcript;
- composer;
- settings;
- MCPs where descriptors exist.

The SDK never receives:

- GPUI Entity, Window, FocusHandle, App, Context, or callback;
- Rust pointers or arbitrary method names;
- OpenSecret credentials/tokens;
- SQLite connections;
- MapleAgentService or backend handles;
- raw secret settings.

The Python worker's filesystem root remains its source task even if it navigates the visible Maple app to another task.

Use tasks consistently as the public product/SDK namespace. Do not ship both tasks and sessions in the first API. A later sessions alias may be added only as a documented compatibility alias.

### 21.5 Bounded discovery and pagination

Every potentially large API accepts scope plus limit and opaque cursor where relevant:

- ui.describe(scope="visible" | target, max_nodes, cursor);
- ui.query(..., limit, cursor);
- actions.list(..., limit, cursor);
- tasks.list(..., limit, cursor);
- transcript.list/focus helpers over a bounded semantic projection.

Initial limits:

| Surface | Initial bound |
|---|---:|
| Semantic nodes per page | 200 |
| Snapshot exposed text per page | 128 KiB |
| Action/query/task results per page | 100 |
| Controller response payload | 512 KiB |
| Semantic event ring | 2,048 events |
| Concurrent event waiters | 64 |
| Maximum event wait | 60 seconds |
| Action audit ring | 512 records |
| In-flight async operation table | 256 |

Return next_cursor when truncated. If a page cannot fit the response cap, reduce it or return result_too_large; never exceed the 1 MiB protocol frame. Expired cursors/event history return resync_required and direct the caller to a fresh describe. Cursors include scope/revision/generation integrity so they cannot silently page through a different task or policy epoch.

### 21.6 Discovery-first behavior

The controller is semantic and introspectable. Models should:

1. describe/query;
2. retain stable IDs and revision/event cursor;
3. inspect action availability;
4. invoke an action;
5. wait for a structured event if completion matters.

They should not:

- click coordinates;
- synthesize shortcut keys;
- sleep/poll when an event exists;
- guess stable IDs;
- cache authority;
- assume completed mutations are rolled back on Stop.

## 22. Built-in maple-ui-controller skill

Current Maple disables Goose's general built-in skills in TrustAwareSkillsClient. Do not turn all of them on.

Add a small Maple-owned embedded skill catalog:

- Advertise maple-ui-controller only when controller access is enabled.
- Intercept its load_skill request.
- Return an include_str-backed trusted SKILL.md.
- Optionally expose it through Maple's slash-command skill listing.
- Omit it entirely when controller is Off.

TrustAwareSkillsClient merges only this Maple-owned descriptor/instruction into its existing catalog; do not enable Goose's other built-ins. Catalog preparation is not authority: intercepting a load_skill call rechecks current Code Mode, controller access, source task, and policy epoch. A skill advertised earlier but loaded after controller disablement returns ControllerDisabled. If access changes mid-turn, any already-loaded prose remains harmless because every SDK call is independently reauthorized.

The skill teaches:

- persistent per-task Python behavior;
- how to import maple_gpui;
- discovery-first workflow;
- stable IDs and action-specific preconditions;
- Read Only versus Full Access;
- events.wait rather than polling;
- structured errors and cancellation;
- no coordinate clicking or key synthesis;
- completed effects are not rolled back;
- small navigation/chaining examples.

The skill grants no capability. It documents a capability Rust already enabled.

## 23. Audit and visible run state

### 23.1 Audit record

Use a bounded in-memory ring initially:

~~~rust
pub struct ActionAuditRecord {
    pub sequence: u64,
    pub invocation_id: InvocationId,
    pub program_id: Option<ProgramId>,
    pub model_run_id: Option<RunId>,
    pub timestamp_ms: i64,
    pub duration_ms: Option<u64>,
    pub actor: InvocationActorSummary,
    pub transport: InvocationTransport,
    pub controller_access: UiControllerAccess,
    pub policy_epoch: u64,
    pub action_id: ActionId,
    pub target: Option<SemanticTarget>,
    pub arguments: RedactedArguments,
    pub effect: ActionEffect,
    pub decision: PolicyDecision,
    pub outcome: AuditOutcome,
}
~~~

Record:

- accepted or denied decision;
- terminal completion/failure/cancel state;
- completed_after_cancel_request when an irreversible race actually occurred.

Do not report async work as completed when it was merely spawned.

### 23.2 Redaction

Redaction is descriptor-driven and deny-by-default.

Safe allowlisted fields may include:

- stable task ID;
- action ID;
- boolean setter value;
- non-secret setting key;
- target kind.

Never retain:

- passwords;
- OAuth callbacks;
- tokens;
- secret headers;
- MCP secret environment values;
- raw account identifiers if a scoped digest suffices;
- arbitrary full Python output in the action audit.

Python code/output already appears in its execution cell/tool timeline. Do not duplicate it into an indefinite action log.

### 23.3 Visible state

Settings > Programmability shows active and retained model/Python UI programs:

- source task;
- Running/Stopping state;
- current or most recent action;
- elapsed time;
- exact program/execution identity where needed for cleanup;
- Stop or Retry Stop;
- bounded Code Mode history and action-audit details.

There is no global HUD. `code_mode.open` provides a stable semantic route back
to the Settings surface.

## 24. Cancellation model

Every model/Python program has:

- host-generated program ID;
- optional owning model run ID;
- execution ID;
- kernel generation;
- CancellationToken;
- current policy epoch;
- bounded action-call budget.

Check cancellation:

- before parsing/validation;
- before policy;
- before UI-thread dispatch;
- before any deferred external effect commits;
- before every later action in a chain;
- before resolving an event wait.

Stopping a program:

- revokes the controller lease immediately;
- prevents subsequent actions;
- cancels event waits;
- interrupts/terminates Python;
- attempts to cancel tracked backend work;
- reports effects that already completed.

For model-triggered Python, Stop also cancels and tombstones the owning Goose
run at the developer-tool boundary, so that run cannot submit a fresh
`python_code` or another controller invocation. Controller calls carry both
actor and transport provenance throughout.

No UI copy should imply transactional rollback.

## 25. Settings persistence and fail-safe behavior

Add serde-defaulted fields to AppSettings for:

- keymap_profile;
- vim_leader;
- one-time Developer Preview disclosure acceptance if needed;
- optional interpreter path preference.

Keep ProgrammabilityAuthority in runtime/account state, not AppSettings, for this Developer Preview. If older experimental fields exist, deserialize them tolerantly but normalize/ignore them to runtime Off/Off.

Requirements:

- Unknown persisted enum values fail only that field to the safer default through tolerant field-local deserialization; unrelated valid settings survive. serde(default) alone is not sufficient if an unknown enum would reject the entire document.
- Invalid keymap JSON leaves the last-known-good keymap active.
- Settings writer remains serialized/coalesced.
- An action result distinguishes local state applied from durable write failed when persistence matters.
- The first Off-to-DeveloperPreview transition completes and durably records the direct-user disclosure before enabling. If that write fails, remain Off. Once disclosure acceptance is durable, session authority transitions do not wait on settings persistence.
- Reductions revoke immediately and atomically normalize the runtime tuple. Because active authority is not persisted and every launch starts Off/Off, a crash/restart after any downgrade cannot resurrect old access.
- The existing serialized/coalesced settings writer must provide an awaited success/failure path for disclosure acceptance; fire-and-forget persistence is insufficient.
- Saved-auth startup may show local chat immediately while validation runs in
  the background. Definitive rejection returns to Login and compare-clears only
  the record that was loaded; timeout/network/server unavailability preserves
  local history and credentials. Code Mode account synchronization waits for
  that restore gate.
- Sign out closes Code Mode admission and confirms account-worker teardown
  before clearing in-memory credentials and compare-clearing the captured
  persisted record. A teardown failure retains the cleanup identity and
  credentials needed to retry instead of reporting a false successful logout.

## 26. Implementation slices and review map

The exact diffs and commit boundaries may shift with integration work, but
preserve these architectural review boundaries.

### Slice 1: Typed action core and semantic control routing

Primary responsibilities:

- maple-harness core types;
- registry and descriptor validation;
- policy matrix and Human Only;
- host-assigned actor/transport provenance;
- structured errors/results;
- audit ring;
- root dispatcher/GPUI bridge;
- initial stable semantic target types;
- migration of every current semantic control;
- checked-in control inventory and direct-callback audit;
- deterministic setters;
- tests for uniqueness, schemas, one-path execution, policy, generic-activation reauthorization, and redaction;
- architecture document in its then-current form.

The slice cannot leave half of the buttons using direct semantic callbacks.

### Slice 2: Keymap profiles and shortcut tooling

Primary responsibilities:

- Standard/Vim templates;
- keymap.json parser/resolver;
- complete atomic GPUI binding reload;
- null unbinding;
- context expansion;
- conflict/prefix reporting;
- Shortcuts settings page;
- shortcut recorder;
- command palette;
- which-key;
- settings persistence;
- tests for references, arguments, precedence, conflicts, recorder, reload, Standard regressions, application-chord/composer-token remapping, and ordinary inputs under the Vim profile.

### Slice 3: Semantic application navigation

Primary responsibilities:

- semantic selection controller;
- stable transcript/sidebar/settings/menu/question/permission targets;
- reveal and reconciliation;
- stream-follow separation;
- application Vim commands and leader;
- ga, bracket a, bracket d, gi, Ctrl-W regions, colon, slash;
- stable semantic events/snapshots;
- tests for virtualized identities and streaming.

### Slice 4: Composer Vim

Primary responsibilities:

- pure composer Vim engine;
- modes, motions, operators, counts, register, Visual, iw/aw;
- explicit edit transactions;
- structured dot repeat;
- TextInput integration;
- mode badge/cursor;
- application/composer transition;
- focused Unicode and regression tests.

### Slice 5: Per-task Python Code Mode

Primary responsibilities:

- maple-code-mode crate;
- interpreter discovery;
- Nix Python/IPython development dependency;
- worker/bootstrap and framed protocol;
- persistent CPython/IPython execution;
- limits;
- process groups;
- Stop/Reset/Restart/cleanup;
- kernel manager keyed by account/task;
- Settings-only Code Mode status, lifecycle, and disclosure surface;
- model python_code tool for general computation;
- lifecycle/cancellation tests.

### Slice 6: maple_gpui controller and end-to-end integration

Primary responsibilities:

- bounded GPUI request/oneshot bridge;
- SDK state/action/event APIs;
- controller Off/Read Only/Full Access;
- dynamic policy epoch/revocation;
- trusted built-in skill;
- audit/run status integration;
- model action chains;
- complete docs and final tests;
- any small live-validation fixes.

Every slice should remain internally coherent and covered by focused tests. The
complete stack must be formatted, warning-clean for every supported feature
set, and pass the full repository test matrix.

## 27. Required automated tests

### 27.1 Action core

- stable ID validation;
- unique descriptors;
- action arguments and result schemas serialize;
- every bindable descriptor has a GPUI adapter;
- every current semantic control reaches the same executor from pointer and typed action paths;
- Read Only allows observe/navigate and denies mutation/external;
- Full Access allows ordinary mutation/external;
- Human Only rejects Model/UserCode actors and Python/Macro/GeneratedUi transports even under Full Access;
- direct pointer/key/palette can invoke Human Only when available;
- programmatic GPUI dispatch cannot mint DirectUser;
- an actual multi-stroke key and a timeout-resolved shorter prefix consume valid window PendingKeyProvenance, while focus change/cancellation clears it and identical App::dispatch_action remains non-direct;
- internal follow-up calls inherit provenance and policy rather than becoming privileged;
- exhaustively test every actor/transport tuple: Human Only allows only DirectUser plus Pointer/Keybinding/CommandPalette, and every Internal tuple is denied;
- controller access changes cannot be controller-invoked;
- permission.respond is denied in Read Only and succeeds/audits in Full Access, including same-run responses as explicitly agreed;
- ui.activate_selected reauthorizes the concrete action;
- relevant target-precondition mismatch returns stale_target while unrelated global streaming revisions do not;
- stable ID to App::build_action to typed adapter reaches the semantic executor;
- one instrumented harmless action invoked through pointer, physical-key GPUI adapter, command palette, model/Python bridge, and generic activation reaches the identical executor identity plus policy/audit hooks exactly once per call;
- the checked-in control inventory has no unexplained semantic callback;
- sensitive fields never appear in audit JSON;
- cancellation prevents subsequent calls;
- async terminal status is recorded correctly.

### 27.2 Keymaps

- Standard and Vim templates never stack;
- every binding resolves to a registered stable action;
- every bound argument validates;
- null becomes an effective unbinding;
- user overrides beat template at equal context depth;
- exact, prefix, shadow, and possible conflicts are classified;
- invalid file preserves last-known-good;
- complete reload restores TextInput/default essentials;
- Vim-profile search/login/rename/settings/question/MCP inputs retain ordinary editing;
- profile switch safely resets modal state;
- recorder captures and cancels correctly;
- command palette uses current availability;
- which-key trie matches current context/prefix;
- Standard current shortcuts and type-to-compose behavior remain.
- user-remapped application chord and composer motion/operator replace the old binding and update which-key/settings.

### 27.3 Semantic navigation

- j/k/gg/G use stable IDs;
- off-screen reveal resolves through current virtual row index;
- a selected streaming item survives revision updates;
- insertion/reordering before selection does not move identity;
- removed selection chooses deterministic neighbor;
- leaving newest disables follow;
- streaming does not repin;
- G restores latest/follow;
- task switch restores valid per-task selection;
- popup contexts shadow application/composer;
- ga and bracket a traverse correct role/kind;
- bracket d returns unavailable with no annotations and works with fixtures;
- gi restores composer insertion;
- generic activation rechecks policy;
- hidden/internal/zero-height timeline records are absent from the navigable projection.

### 27.4 Composer Vim

All tests listed in Section 17.13 are required.

### 27.5 Worker/core

- fragmented and oversized frames;
- input()/raw stdin cannot consume control frames;
- handshake/version mismatch;
- complete envelope/state-machine validation for handshake, execute, sdk_call/result, cancel, controller change, reset/restart, shutdown, and terminal messages;
- protocol descriptors are non-inheritable by ordinary subprocesses before user code runs;
- duplicate/out-of-order/stale generation messages;
- CPython fallback;
- optional IPython selection;
- persistent variables/imports;
- top-level await;
- raw Python, os.write, native/subprocess stdout/stderr, display, final expression, exception, barrier ordering, and post-barrier contamination;
- sdk_result completes while the interpreter main thread awaits it;
- concurrent controller downgrade/cancel/shutdown reaches the protocol reader during execution;
- reset/restart state clearing;
- source/output/frame/time bounds;
- infinite loop cancellation;
- infinite output cancellation;
- worker crash/protocol corruption recovery;
- direct-worker reaping, process-group/job cleanup, and best-effort tracked-descendant cleanup;
- Python-managed thread/tracked-subprocess contamination forces restart, while native escape limitations are disclosed;
- per-task and per-account isolation;
- task root verification;
- eight-kernel cap/idle eviction and capacity_exceeded for a ninth request while all run;
- task deletion/Clear Scratch races and exact scratch target validation;
- env_clear/minimal allowlist with known credential, proxy, loader-injection, and auth variables absent;
- shutdown reaps every direct worker and reports any observable tracked-descendant cleanup failure.

Use two test tiers: deterministic protocol/supervision tests with a fixture worker, and native CPython/IPython integration tests inside the Nix environment. If IPython is unavailable in a non-Nix CI job, report that optional engine test as explicitly skipped; do not silently count a fixture as native coverage.

### 27.6 Agent/model integration

- python_code absent when Off;
- general Python succeeds with controller Off;
- source task project root is used even when another UI task is selected;
- Goose cancellation reaches the worker;
- Maple-owned model run IDs are unforgeable, cancel_run reaches the owning generation, and terminal cleanup removes/tombstones the registry entry;
- model Stop cancels/tombstones the owning run so it cannot call python_code or controller actions again;
- logout/task deletion clean up kernels;
- stale tool calls recheck current policy;
- tool result is bounded;
- maple-ui-controller skill appears only when controller enabled;
- a skill advertised before disablement cannot be loaded afterward;
- sequential model executions demonstrably share the documented namespace,
  including Reset/Restart behavior.

### 27.7 SDK/controller

- Read Only describe/query/navigation succeeds;
- Read Only mutation fails;
- Full Access ordinary mutation succeeds;
- permanent account/identity Human Only actions fail under Full Access;
- generic activation cannot bypass;
- schema and stale revision errors are structured;
- pagination, result_too_large, expired cursor/resync, and response caps work;
- bridge queue-full/enqueue-timeout/UI-closed/app-quitting/dropped-response paths are structured and release waiters;
- an SDK request wakes and completes against a totally idle GPUI loop with no render/input event;
- no service/kernel/global lock is held while awaiting GPUI;
- downgrade while a call is queued fails closed;
- no SDK call occurs after cancellation;
- describe redacts secure fields;
- events.wait is sequence-safe, bounded, timed, and cancellable;
- retained SDK object after downgrade has no authority;
- a cell returning accepted retains a visible/stoppable ProgramRecord until every handle terminates; Stop/downgrade/Reset/Restart and a later new execution obey the lifetime contract.

### 27.8 Settings

- defaults are Standard, Code Mode Off, controller Off;
- unknown values fail safe;
- disclosure is required;
- controller cannot enable while Code Mode Off;
- disabling immediately revokes active work;
- keymap parse failure is visible and non-destructive;
- settings write errors are surfaced;
- first disclosure write failure leaves runtime Off;
- every downgrade revokes immediately, and crash/restart/account change always returns runtime authority to Off/Off;
- a stale/unknown legacy Code Mode/controller field cannot activate authority or discard unrelated settings.

## 28. Repository validation

Run focused tests throughout. Before upstream review, run the repository's
complete single-host checks from the Nix environment. Release/live validation
is a separate gate and should be repeated in proportion to code changes; an
upstream rebase or documentation-only follow-up does not require rebuilding a
previously proven release artifact merely to request architectural feedback.

The repository currently defines:

~~~text
just fmt
just ci
just release
~~~

`just ci` is the authoritative single-host pre-commit command. It covers:

- cargo fmt check;
- the strict semantic-control inventory;
- Clippy for the default, combined headless, ACP-only, and proxy-only feature
  sets with warnings denied;
- workspace build/tests;
- headless tests.

It does not reproduce GitHub's cross-OS matrix or separate Linux release job.
`just release` is also separate.

Also run focused package/test commands that make failure diagnosis legible.

Do not treat a zero-test filter as validation.

### 28.1 Nix/macOS

Use the repository flake and external Xcode/Metal development path. `just` is
a repository prerequisite and must already be available on PATH; the flake
currently supplies the pinned build/runtime dependencies but not `just` itself.

~~~text
MAPLE_NIX_XCODE_VERSION=<installed-version> nix develop --no-update-lock-file --command just ci
MAPLE_NIX_XCODE_VERSION=<installed-version> nix develop --no-update-lock-file --command cargo build --release -p maple-gpui --locked
~~~

On macOS the shell honors `MAPLE_NIX_XCODE_VERSION` first, then a valid
inherited `DEVELOPER_DIR`, `/Applications/Xcode.app`, and finally the
installation selected by `xcode-select`. It derives `SDKROOT` with
`/usr/bin/xcrun` and selects that Xcode's compiler for native dependencies.
Verify the optional Metal toolchain as documented in README. Keep the pure Nix
build/runtime-shader path working where practical.

Release builds use fat LTO and one codegen unit. They can be quiet and memory-heavy for a long time. Do not declare a hang while rustc/linker processes are active. Be kind to the big-memory LTO.

Adding Python/IPython to the dev shell must preserve:

- aarch64-darwin;
- aarch64-linux;
- x86_64-linux;
- pure package behavior where Python is not yet bundled into the app output.

The macOS release build and GUI behavior are the primary gate. Keep non-desktop/headless feature sets compiling.

## 29. Live macOS validation

Automated tests are not sufficient for the implementation milestone. Launch
the exact newly built macOS app and exercise that app through direct or
automated UI interaction.

Record:

- git commit;
- binary path and hash;
- running process executable path;
- interpreter path/version;
- enabled profile/controller;
- visible screenshots or precise UI observations.

Controller correctness must not depend solely on external model credentials.
First exercise the same broker/SDK path with an in-process or deterministic
agent/controller fixture, then perform the live model-driven exercises when a
configured model service is available. If it is unavailable, report that exact
limitation and the fixture evidence; do not silently waive or pretend the
model exercise ran.

Do not grant macOS privacy/security permissions during validation without a new direct instruction.

### 29.1 Settings and shortcuts

1. Open Settings.
2. Verify Keyboard Shortcuts and Programmability sections.
3. Search actions by label, stable ID, and key.
4. Switch Standard to Vim and back.
5. Confirm Standard shortcuts remain ordinary.
6. Record a custom binding.
7. Create/inspect a conflict.
8. Disable with null.
9. Reset one action and the profile.
10. Verify unbound/conflict filters.
11. Open the command palette.
12. Trigger a leader sequence and see which-key.
13. Verify portable Secondary shortcuts resolve to Cmd on macOS.
14. Exercise native project and attachment pickers, including cancellation;
    verify explicit controller paths do not open a picker.

### 29.2 Application Vim

1. Navigate task/project/sidebar rows with j/k/h/l.
2. Open a task with Enter.
3. Navigate transcript with j/k/gg/G.
4. Verify selected row highlight and off-screen reveal.
5. Use ga and bracket a.
6. Verify bracket d is honestly unavailable if no annotation objects exist.
7. Use Ctrl-W region movement.
8. Use colon palette and Space leader.
9. Start/observe streaming, navigate away, and verify it does not steal selection or repin.
10. Use G to return to newest/follow.
11. Use gi to return to the stored composer insertion, then replace the draft
    externally and verify gi safely lands at the current draft end.

### 29.3 Composer Vim

Exercise, at minimum:

- Insert/Normal/Visual transitions;
- h/j/k/l, w/b/e, 0/dollar, gg/G;
- i/a/I/A/o/O;
- dw, d$, dd;
- cw, ciw, cc;
- yw, yiw, yy;
- x, p/P;
- counts such as 3w, 2dd, 2d3w;
- u and Ctrl-R;
- Visual yank/delete/change;
- ciwhello Escape followed by dot on another word;
- A! Escape followed by dot;
- Standard profile typing after switching back;
- slash palette/history/send/Shift-Enter regressions.

Accessibility automation may not perfectly emulate Vim timing. Supplement UI
automation with deterministic engine/integration tests and report which
behaviors were directly observed.

### 29.4 General Python with controller Off

Use sequential model `python_code` calls to:

- assign a variable and use it later;
- print stdout and stderr;
- execute top-level await;
- raise and display an exception;
- Reset and verify state clears in a later call;
- Restart and verify generation changes in Settings;
- create/read a fixture in the task project;
- create/read scratch metadata;
- attempt outside-project read and a socket connection.

For the native Developer Preview, any successful escape must match the warning and report, not contradict UI claims.
Verify execution history, interpreter state, and lifecycle controls only in
Settings > Programmability; no chat REPL or global Code Mode HUD should appear.

### 29.5 Controller Read Only

Ask the model through python_code to:

- import maple_gpui;
- describe current state;
- list/search actions;
- list tasks;
- open/navigate a task or transcript item;
- wait for a semantic event;
- attempt a settings mutation and receive policy_denied;
- attempt sign out and receive policy_denied.

Verify the UI visibly follows navigation where appropriate.

### 29.6 Controller Full Access

Exercise reversible ordinary actions:

- open Settings/Shortcuts/Programmability;
- switch a view/section;
- set a reversible ordinary preference and restore it;
- open another task;
- send a harmless test message only in a task expressly used for validation;
- inspect/configure an MCP only if a reversible local test fixture exists;
- exercise permission.respond against an expressly created harmless validation prompt and verify it succeeds with conspicuous Full Access audit provenance;
- verify sign out and controller/Code Mode self-escalation remain denied from Python;
- as the final controller exercise only, verify app.quit returns/records accepted_terminal before orderly shutdown, then relaunch for the lifecycle checks if needed.

Use discovery and action invocation, not coordinates/keys.

### 29.7 Stop and lifecycle

1. Start an infinite loop.
2. Open Settings > Programmability and press the matching Stop control.
3. Verify UI remains responsive.
4. Verify worker/process group disappears or restarts.
5. Run a later model `python_code` call successfully.
6. Start an output flood and repeat.
7. Use two tasks and verify separate namespaces/scratch/root.
8. Downgrade controller during a wait/pending call and verify revocation.
9. Sign out directly and verify all account kernels stop.
10. Quit and verify no worker process remains.
11. Relaunch and verify runtime Code Mode/controller authority is Off/Off even if it had been Full Access before quit.
12. Inspect audit entries for actor, transport, action, decision, result, and redaction.
13. Relaunch with saved credentials and verify optimistic local-chat restore,
    background validation, and subsequent Code Mode account synchronization.

## 30. Review and promotion gates

Before proposing promotion beyond Developer Preview:

- the branch and worktree are clean and based on current upstream;
- `just ci` passes, including the strict semantic-control inventory;
- release-mode compilation succeeds;
- the exact built process is launched and identified;
- Settings, Standard shortcuts, application/composer Vim, Python Code Mode,
  and model-driven `maple_gpui` workflows are exercised live;
- platform-specific validation is identified separately from portable
  compile/test evidence;
- deviations from this design are documented with their rationale;
- production-hardening TODOs remain explicit rather than being hidden behind
  Developer Preview language.

An upstream proposal should summarize the architectural context, review map,
exact checks, live behaviors, honest Python boundary, and non-goals. Opening a
review does not authorize a merge, release, signing, or installation.

## 31. Acceptance checklist

The experiment is complete only if all are true:

- [ ] One action executor serves pointer, shortcuts, palette, Vim, model, and Python.
- [ ] Every current semantic control is registered or explicitly justified as local gesture mechanics.
- [ ] Actions have stable IDs, typed args/results, documentation, availability, effect, policy, and recoverability.
- [ ] Actor, transport, and controller authority are host-assigned.
- [ ] Read Only cannot mutate.
- [ ] Full Access can perform ordinary controller-callable mutations without per-action prompts.
- [ ] Permanent account/identity Human Only actions reject every non-direct actor/transport, including generic activation.
- [ ] Agent tool permission remains a separate setting, while Full Access permission.respond behavior matches the explicit product decision and is audited.
- [ ] Stable semantic selection survives virtualization and streaming.
- [ ] Standard and Vim are replacement templates with arbitrary overrides.
- [ ] Shortcut settings, recorder, conflicts, palette, and which-key work.
- [ ] Application Vim covers chat, transcript, sidebar, tasks/projects, settings, menus, questions, permissions, tools, and annotation-ready navigation proven by fixtures; a live app with no annotation producer honestly reports unavailable.
- [ ] Composer Vim includes every agreed command and structured dot repeat.
- [ ] Standard inputs remain unaffected.
- [ ] Python is persistent per account/task and outside GPUI.
- [ ] Python has bounded protocol/output/runtime/kernels and working Stop/Reset/Restart.
- [ ] General Python works with controller Off.
- [ ] maple_gpui discovery/action/event chaining works.
- [ ] Current policy is rechecked on every SDK action.
- [ ] Runtime Code Mode/controller authority is one normalized session tuple and relaunch/account change starts Off/Off.
- [ ] Built-in controller skill is present only when enabled.
- [ ] Security disclosure is honest.
- [ ] just ci and release build pass.
- [ ] Exact release app was launched and validated through UI.
- [ ] Upstream review material explains the context, architecture, validation, and remaining hardening without claiming production readiness.

## 32. Future direction

Once this vertical spine is proven, the most interesting next layers are:

1. Saved named action programs/macros bindable like ordinary actions.
2. Transactional or parallel composition semantics.
3. Hardened native Code Mode helpers.
4. Durable Python environments and RLM orchestration.
5. Versioned declarative generated UI.
6. Capability-limited extensions.
7. Multi-window and remote-session semantic control.

The core thesis should remain:

> Maple is not a GUI with AI automation bolted onto it. Maple is a programmable, typed, semantic application whose human and model interfaces are clients of the same harness.

## 33. Developer Preview implementation boundaries

The Settings-only placement is incorporated throughout the normative sections
above. The implementation also records two proof-of-concept boundaries that
must remain explicit during review and production hardening.

### 33.1 Developer Preview teardown receipt

The proof-of-concept implementation closes Code Mode admission before an Off
transition, drains already-published admission reservations, and captures the
exact active program/execution identities from every responsive account-kernel
actor that is successfully observed before worker interruption begins.
Settings unions that atomic backend receipt with the last account-wide status
poll and retained `ActionHost` ProgramRecords.
This prevents a failed Off transition from losing a pure Python program or a
retained controller program merely because the 250 ms presentation poll had
not observed it yet. Stale account/policy completions cannot merge into or
retarget a newer teardown generation.

One Developer Preview boundary remains for production hardening: the worker
service removes kernel handles and the actor exits after its bounded shutdown
attempt even when process cleanup returns diagnostics. `Retry Stop` therefore
revalidates the retained exact identity and clears a target that is already
terminal, but it is not guaranteed to issue a second operating-system cleanup
attempt against the same handle. A production implementation should retain a
separate retry-addressable cleanup owner until process-group termination is
positively confirmed.

The proof-of-concept receipt capture is serial and has no per-actor timeout,
and the receipt is returned to one transition waiter. Production hardening
should capture actor status concurrently with explicit bounds and durably own
the receipt across waiter cancellation, task failure, or process interruption.

### 33.2 GPUI pointer-origin limitation

GPUI 0.2.2 exposes window-wide pointer capture, which the Developer Preview
uses so occluding popups cannot bypass the root provenance boundary. Its public
`PlatformInput` representation does not distinguish native hardware input from
an in-process synthetic dispatch, however. The preview therefore proves a
bounded, single-use GPUI pointer-dispatch capability, not operating-system
attestation of a physical device event. The controller/model/Python surfaces
have no API to synthesize GPUI input, and programmatic semantic action dispatch
still receives no token. Production hardening should add an origin-bearing GPUI
API or mint direct-user provenance below the public synthetic dispatch seam.
