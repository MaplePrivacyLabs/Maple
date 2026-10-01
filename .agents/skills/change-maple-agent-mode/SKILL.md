---
name: change-maple-agent-mode
description: Develop and debug Maple Agent Mode across its React UI, Tauri command and event bridge, account-scoped native runtime, embedded Goose integration, OpenSecret provider, developer tools, permissions, skills, MCP, and ACP adapter. Use when implementing or debugging AgentMode, agent runtime lifecycle or persistence, Goose pins or prompts, model/tool streaming, cancellation, local filesystem or shell tools, project trust, MCP servers, external ACP callers, or Agent account isolation.
---

# Change Maple Agent Mode

Work from the Maple repository root. Read root `AGENTS.md` and `apps/maple-research/AGENTS.md`, the current source, nearby tests, and relevant docs before editing. Treat `apps/maple-research/frontend/src-tauri/Cargo.toml`, `apps/maple-research/frontend/src-tauri/Cargo.lock`, the implementation, and test suite as authoritative when historical prose disagrees.

Root `AGENTS.md` owns shared policy; `apps/maple-research/AGENTS.md` owns app setup and runtime conventions. Use
`$validate-maple` for complete CI-equivalent, packaged, or cross-platform
validation, and `$review-maple-security` when a change touches credentials,
authorization, privileged tools, process execution, local IPC, or another
trust boundary.

Do not commit, push, open a PR, merge, release, or clean unrelated work unless the user explicitly requests it.

## Understand the Runtime Before Changing It

Keep Maple's two inference paths distinct:

- Research chat is a browser-capable React path. `UnifiedChat.tsx` uses the OpenAI JavaScript client with `@mapleai/sdk`'s authenticated and encrypted `aiCustomFetch`, targeting OpenSecret's Conversations and Responses APIs.
- Agent Mode is desktop-only. `AgentMode.tsx` calls typed frontend services, which invoke Tauri commands and receive `agent-event` envelopes. Native Rust owns `MapleAgentService`, the embedded Goose managers and local sessions, Maple's provider, developer tools, permissions, and cancellation. `MapleProvider` sends encrypted OpenSecret SDK requests to `/v1/chat/completions`.
- ACP is a secondary local adapter over the same `MapleAgentService`. It must not initialize a second Goose runtime or call through the Tauri adapter.
- The local OpenAI-compatible proxy is a separate user-facing feature. Neither Research chat nor Agent Mode should silently route through it.
- Agent Mode, Goose, native OpenSecret auth, MCP, and ACP compile only for desktop. Mobile retains shared React functionality and selected native facilities such as document extraction, not Agent Mode.

Maple locally persists Agent tasks, project configuration, MCP snapshots, and Goose session history under an opaque account scope. OpenSecret remains authoritative for account identity, authentication, encrypted inference, model/backend policy, and web search or extraction. Do not confuse local Agent history with server-persisted Research conversations.

## Put Changes at the Owning Boundary

Use these seams:

- `apps/maple-research/frontend/src/components/AgentMode.tsx` and `apps/maple-research/frontend/src/components/agent/`: presentation, interaction state, accessible controls, and projection of native events. Do not enforce security only here.
- `apps/maple-research/frontend/src/services/agentRuntimeService.ts`: typed Tauri commands/events, user-operation fencing, and logout/account-transition coordination.
- `apps/maple-research/frontend/src/services/mapleApiAuthService.ts`: browser/native credential reconciliation. Do not copy this logic into components.
- `apps/maple-research/frontend/src-tauri/src/agent_tauri.rs`: the stable Desktop command and event adapter. Keep it thin.
- `apps/maple-research/frontend/src-tauri/src/agent_host.rs`: operations spanning the core runtime and ACP, including stop, restart, clear, exit, and update shutdown.
- `apps/maple-research/frontend/src-tauri/src/agent.rs`: transport-neutral Agent domain state, account handles, Goose/session orchestration, persistence, run ownership, timelines, MCP and skills attachment, and permission routing. Do not import Tauri or ACP protocol types into this core.
- `apps/maple-research/frontend/src-tauri/src/agent/provider.rs`: Goose-provider request construction, encrypted inference transport, streaming, bounded error handling, retry policy, and cancellation.
- `apps/maple-research/frontend/src-tauri/src/maple_api.rs`: account-scoped native OpenSecret SDK sessions, backend identity validation, atomic credential replacement, and refresh reconciliation.
- `sdk/rust/`: source for the Maple Rust SDK. Research selects a published
  version or local source through `apps/maple-research/frontend/src-tauri/Cargo.toml`
  and its lockfile. Follow the [consumer version policy](../../../docs/sdk-publishing.md#consumer-version-policy)
  and verify the resolved graph before claiming an SDK source edit reaches
  Agent Mode; a registry-pinned build does not exercise that edit.
- `apps/maple-research/frontend/src-tauri/src/agent/developer_tools.rs`: Maple's privileged read, write, edit, image, shell, and backend web-tool implementations. Add privileged tools here rather than exposing an unmediated Goose tool path.
- `apps/maple-research/frontend/src-tauri/src/agent/{shell_permission,web_permission,tool_context,web_tools}.rs`: automatic policy, secret-bearing execution context, public-URL admission, provenance, and output bounds.
- `apps/maple-research/frontend/src-tauri/src/agent/system_prompt.rs`: the narrowly rebranded copy of the pinned Goose system prompt.
- `apps/maple-research/frontend/src-tauri/src/agent_acp.rs`: ACP framing, Unix socket ownership, connection/session leases, caller-owned permissions, and adapter-specific environment allowlists.

Put behavior in OpenSecret instead when it must be authoritative against a modified client, shared by multiple clients, durable across devices, or part of the public API, authentication, confidential-compute, model, provider, search, or extraction contract. Coordinate the open-source backend and the in-tree SDK under `sdk/` rather than emulating missing enforcement in Maple.

## Preserve Trust and Lifecycle Invariants

### Account and credential isolation

- Bind every native operation to the requested user's opaque account scope and current handle generation. Reject stale handles and cross-account sessions.
- Validate candidate native credentials against the backend before publishing them. Preserve atomic replacement and prevent a late refresh from overwriting a newer browser or native token pair.
- Synchronize auth before authenticated Tauri calls. On logout or account switch, block new operations, drain Agent/ACP, then clear native auth and account-bound local proxy credentials.
- Never put OpenSecret access or refresh tokens into Goose configuration, prompts, tool arguments, child environments, logs, or error strings.

### Runtime, session, and run lifecycle

- Keep one authoritative native runtime for the signed-in account. Serialize start, stop, restart, clear, exit, and update operations across Agent and ACP.
- Claim a session before changing its title, model, permission mode, extensions, or tool context. Reject concurrent mutation rather than racing it.
- Define ownership for every spawned task, listener, stream, permission request, cancellation token, and secret-bearing context. Ensure every terminal path cancels, joins, revokes, persists or repairs history, emits an authoritative terminal event, and removes active state in the intended order.
- Fence frontend async work by account, session, run, and revision/generation. Navigation may change the visible task but must not redirect an existing run's events.
- Preserve the session model lock after history exists. Treat a model change as a new task unless product requirements explicitly change that contract.
- Keep bounded event queues fail-loud. Never let Desktop or ACP backpressure Goose indefinitely or silently present a truncated stream as complete.

### Permissions and privileged tools

- Keep Goose live in `SmartApprove` so every sensitive tool reaches Maple's policy boundary. Persist the user-facing `Read only` (`smart_approve`) or `Allow all` (`auto`) choice separately.
- Reset Maple's Goose permission file so `read`, `shell`, `edit`, `write`, `read_image`, `web_search`, and `open_url` route through `ask_before`; only `load_skill` is always allowed.
- Derive current policy per tool from its permission configuration, automatic
  handler, classifier, and executor. Distinguish always-allowed, automatic,
  classifier-approved, and caller-approved paths; do not generalize one tool's
  classifier or URL policy to another.
- Automatic policy may approve only explicitly defined operations. On a
  classifier-backed path, timeout, failure, or ambiguity falls back to caller
  approval; otherwise follow the explicit per-tool policy.
- Preserve one-shot, run-scoped permission capabilities. The calling surface owns unresolved permissions: Desktop for Desktop runs and ACP for ACP runs. Never expose one actionable request to both.
- Bound shell time and output. Treat process-group or job-object cleanup as
  best-effort containment of the current child tree, not a hard credential
  boundary. On normal cancellation, revocation, timeout, or overflow, revoke
  the context, request tree termination through the owned process-group or job
  handle, and reap the wrapped child. If hard non-survival is required, keep
  credentials out of arbitrary child environments.
- Treat paths, shell commands, MCP definitions, headers, environment values, model output, URLs, extracted pages, and tool results as untrusted. Revalidate at the native enforcing boundary.
- Admit `open_url` only for normalized public HTTPS URLs. Preserve private, loopback, link-local, metadata, credential-bearing, and malformed URL rejection, bounded output, per-session provenance, and explicit untrusted-evidence notices.

### Skills, projects, MCP, and ACP

- Keep project roots canonical and account-scoped. Project removal, ordering, history, and trust decisions must not mutate another account or silently re-add a removed root.
- Load project instructions and skills only after the user trusts that canonical project. Keep the skills client transient so Goose cannot reconstruct it later with an untrusted real working directory.
- Validate and freeze MCP definitions into each session. Preserve reserved names, unsafe-environment rejection, header normalization, owner-only storage where supported, and the warning that obscured stored values are not encrypted at rest.
- Keep ACP disabled by default and Unix-only until its implementation changes. Require absolute admitted roots, owner-only socket access, bounded frames/connections/outbound work, exact session leases, and explicit environment allowlists. Revocation or disconnect must make captured tool context unusable.
- Never describe `read_only` ACP configuration as a sandbox. It is caller-mediated permission policy; a caller can still approve mutation.

### Provider and privacy behavior

- Preserve authenticated encrypted transport through the OpenSecret SDK. `MapleProvider` must not own token storage or refresh.
- Retry only failures that are safe before the first successful streamed item. Do not repeat deterministic client failures or side effects speculatively.
- Keep parser errors, HTTP bodies, provider details, decrypted SSE data, prompts, tool results, and private file contents out of logs and user-facing error strings. Return bounded, categorized errors.
- Preserve cancellation through Goose, provider transport, SDK operations, tools, and terminal history repair.

## Follow the Change Workflow

1. Inspect `git status --short --branch`, current SHA, `origin/master`, and the complete existing diff. Preserve unrelated work and do not switch branches over a dirty tree.
2. Trace the request through UI, frontend bridge, Tauri adapter, core service, Goose, provider/tools, persistence, and backend. Identify which boundaries do not need to change.
3. Write down the account owner, session owner, run owner, permission owner, cancellation path, persistence point, terminal event, and cleanup path for the proposed behavior before editing concurrent code.
4. Add or update the smallest transport-neutral core behavior first, then adapters/wire types, then UI projection. Keep adapters from reaching through one another.
5. Add regression tests for success plus stale account/session/run, cancellation, duplicate/concurrent calls, partial failure, restart/reload, and bounded-input behavior relevant to the change.
6. Update public docs when behavior or compatibility changes. Mark historical smoke evidence as historical and never copy an old dependency pin into current instructions.
7. Inspect the final diff for secret logging, unbounded data, bypassed permissions, detached tasks, stale completion, cross-account access, and behavior that belongs in OpenSecret.

For a Goose bump, obtain the exact revision from
`apps/maple-research/frontend/src-tauri/Cargo.toml`, inspect that revision in a real git checkout,
update `apps/maple-research/frontend/src-tauri/Cargo.lock`, and compare
`apps/maple-research/frontend/src-tauri/src/agent/system_prompt.rs` byte-for-byte with the pinned
Goose `crates/goose/src/prompts/system.md`. Only Maple's two identity lines may
differ. Re-run the prompt-drift test and every affected provider, permission,
tool, MCP, session, and lifecycle test. Do not reason from an unversioned source
archive or current Goose `main`.

## Run Targeted Tests

Run the smallest relevant frontend set without Bun's automatic dotenv loading:

```bash
nix develop --no-update-lock-file .#ci -c bash -c \
  'cd apps/maple-research/frontend && bun --no-env-file test ./src/components/AgentMode.test.ts'
```

Add only the service tests matching the changed boundary, such as runtime/auth
lifecycle, operation fencing, timeline, models, MCP, project/session selection,
or connection availability.

Run Rust tests by affected module while iterating:

```bash
nix develop --no-update-lock-file .#ci -c bash -c \
  'cd apps/maple-research/frontend/src-tauri && cargo test --locked "agent::provider::tests::"'
```

Replace the example filter with the affected module and add adjacent filters
only when their boundary changed. Run the complete locked Rust suite before
handing off a native change. For any Agent Mode change, also run formatting,
linting, typechecking, and builds required by `$validate-maple`. Unit tests do
not replace a real desktop smoke.

## Perform the Exact Desktop Smoke

Choose the [environment/login path](../../../docs/development-environments.md)
for the change. Backend changes and specific client/backend/log/billing
interactions use an isolated linked Local stack and supported encrypted local
password/account fixtures. Intentional small frontend Dev work needs a valid
Dev account/login. Use disposable accounts and non-sensitive fixtures. In a
standalone checkout, if remote rollout has not enabled Agent Mode for that
account, set `VITE_FORCE_FEATURE_FLAGS=agent_mode` in the untracked
`apps/maple-research/frontend/.env.local` and restart the frontend server. Do not edit an
externally managed environment file; use its owning environment's configuration
path or report the scenario unavailable.

1. Record the commit SHA, `VITE_OPEN_SECRET_API_URL`, selected model ID when relevant, build profile, and whether `.local/tauri-workspace.json` is active. Record the effective Tauri identifier, dev URL, exact executable/application path, frontend-server PID, and native PID.
2. When the scenario needs project content, create a disposable project directory with a `README.md` containing a unique marker. Record the exact directory so cleanup cannot target a broader path.
3. Read the active Tauri `devUrl` and inspect its listener before launch. If it
   is occupied, stop the process only when you can prove it belongs to this
   checkout, using its owning lifecycle mechanism where one exists; otherwise
   use another overlay or port. Launch from the repository root with
   `nix develop --no-update-lock-file -c just desktop-dev`. Operate the native window belonging to
   the recorded PID and identifier, never a browser tab or another installed
   app named Maple.

Every Agent Mode UI change requires an exact native-app smoke because the web
build does not exercise this desktop-only path. Keep the smoke proportional to
the changed boundary and classify it by behavior, not file path:

- For presentation-only changes confined to icons, copy, spacing, or markup for
  already-derived timeline state, exercise the exact changed states and
  interactions in the native app. Sign in, open Agent Mode, and navigate only
  as far as the changed state requires. Check relevant loading, streaming,
  completed, error, or permission presentation plus keyboard, focus,
  accessibility, theme, and layout behavior as applicable. Use a minimal
  read-only prompt when runtime data is needed. Do not create files, approve
  shell execution, switch accounts, or run restart/logout lifecycle exercises
  unless the changed UI depends on those boundaries. If event mapping, state
  derivation, timeline grouping or merging, status transitions, action handlers,
  IPC calls, permission callbacks, or account/session/run ownership changed,
  this is not presentation-only. If permission presentation itself changed,
  trigger one disposable request, deny it, and verify that no mutation occurred.
- For runtime, tool, permission, cancellation, persistence, task ownership,
  session switching, restart, logout, app-exit, authentication, or account
  isolation changes, run the applicable full-baseline steps below. Run the
  complete baseline when the change is cross-cutting or its affected lifecycle
  boundaries cannot be isolated. For a narrower change within one of these
  areas, run the affected steps and explicitly report what was not exercised.

For changes requiring full lifecycle coverage, continue through the remaining
baseline:

4. Sign in, open Agent Mode, select the disposable project, choose the intended model, keep `Read only`, and start a new task. Codex must use its built-in Computer Use instead of CUA, even when CUA is installed. CUA only: the native picker is not a Maple window — see `$validate-maple` **CUA only: macOS Open/Save**.
5. Send: `Read README.md using the read tool. Reply only with the exact marker. Do not use shell.` Confirm the visible user row, tool activity, exact marker result, final answer, and completed terminal state.
6. Send: `Create agent-smoke-output.txt containing exactly <marker> using the write tool. Do not use shell.` Confirm a single pending permission card. Choose `Deny once`; verify the card settles as denied, the run terminates coherently, and the file does not exist.
7. Repeat the write request and choose `Allow once`. Verify one settled permission card, exact on-disk contents, a visible tool result, final answer, and completed state. Do not use `Allow all` merely to make the smoke easier.
8. Send: `Run the shell command sleep 30 and do nothing else.` Approve the shell request, wait until the tool is running, then press Cancel. Confirm the UI reaches a cancelled terminal state, no permission remains actionable, a reload shows repaired coherent history, and process inspection shows no shell descendant owned by that run after cancellation.
9. Create a second task, switch between tasks during a streamed response, and confirm events remain with their owning task. Relaunch the exact app and verify local task order/history plus the selected task restore without duplicating the terminal message.
10. Exercise restart, logout, and app exit. Confirm active work is drained, native authentication is cleared on logout, no old-account runtime or tool context can be used afterward, and no Agent/ACP child or listener from the exact tested app remains after exit. If account isolation changed, repeat with a second disposable account and verify neither account can see or mutate the other's tasks/configuration.

Use steps 4–5 for core run, stream, and completion behavior; 6–7 for write and
permission behavior; 8 for shell cancellation and descendant cleanup; 9 for
task ownership, switching, persistence, and relaunch; and 10 for restart,
logout, app exit, authentication, and account isolation.

After the applicable baseline coverage, add boundary-specific proof:

- MCP: follow `apps/maple-research/docs/agent-mode-mcp.md` with its pinned Everything-server fixture over each affected transport; verify the unique marker through request, arguments, result, answer, disable, and failure paths.
- ACP: follow `apps/maple-research/docs/agent-mode-acp.md`, but re-check the live Cargo pin and implementation first. Verify socket ownership, allowed-root rejection, caller-only permission routing, disconnect/cancel cleanup, and bounded overflow behavior with the actual client in scope.
- Provider or streaming: exercise first-byte failure, partial stream, cancellation, context-limit failure, and reload without exposing decrypted bodies.
- Auth or lifecycle: exercise token refresh, logout during a run, account switch, restart, and app exit while inspecting only sanitized logs and exact owned processes.
- Skills or project trust: test trusted and untrusted canonical roots, restart, removal, and account isolation.

Clean up only the exact disposable project, account data, processes, listeners, and test server instances created for the smoke. Report automated, artifact, runtime, integration, and untested evidence separately. Never call a web build, Rust test, or successful app launch an Agent Mode end-to-end pass.

## Hand Off the Change

Report the behavior and owning boundary, affected account/session/run and trust invariants, files changed, exact commands and results, exact native smoke identity and observations, backend/model configuration used, and every untested platform or integration. State whether the tree remains uncommitted and unpushed.
