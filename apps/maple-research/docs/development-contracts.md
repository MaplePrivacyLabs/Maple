# Research implementation contracts

Read the sections relevant to runtime placement, privileged native changes,
account isolation, or concurrency. Source paths and commands use the monorepo
root. Shared policy stays in [root AGENTS.md](../../../AGENTS.md).

## Product and runtime map

Maple is a React/Vite application packaged with Tauri for desktop and mobile.
OpenSecret is its required backend. Keep these runtime paths distinct:

- Research chat: React -> OpenAI JavaScript client ->
  `@mapleai/sdk` encrypted custom fetch -> OpenSecret Responses and
  Conversations APIs.
- Desktop Agent Mode: React -> Maple-owned Tauri commands/events ->
  `MapleAgentService` -> embedded pinned Goose -> `MapleProvider` -> Rust
  OpenSecret SDK -> `/v1/chat/completions`.
- ACP: an external client edge over a protected local socket into the same
  Maple Agent service. ACP is not Agent Mode's internal abstraction.
- Local proxy: a separate user-facing OpenAI-compatible relay. Research chat
  and Agent Mode do not internally route through it.

The OpenSecret SDK source lives under `sdk/`. Research independently selects
published TypeScript and Rust versions in its manifests and lockfiles; local
`file:`/Cargo links are supported for active development. Follow the
[SDK consumer version policy](../../../docs/sdk-publishing.md#consumer-version-policy).
Do not assume the TypeScript and Rust SDKs have identical transports, retries,
or API coverage. A backend contract change that Maple consumes needs compatibility
checks for every affected client path.

The Maple Proxy source lives under `proxy/`. From the repository root,
run its Rust commands through
`nix develop --no-update-lock-file ./proxy -c bash -lc 'cd proxy && ...'`;
root path-scoped workflows own proxy CI. Proxy runtime changes select desktop
builds; SDK-only changes do not, since Research builds the SDK source its own
manifests select. Container, test, documentation, and standalone lockfile
changes remain independent.

## Code ownership and placement

- `apps/maple-research/frontend/src/routes`, `components`, `contexts`, and `state` own routing,
  presentation, account-scoped UI state, drafts, and interaction behavior.
- `apps/maple-research/frontend/src/services` owns browser-side API/Tauri bridges and lifecycle
  orchestration. It is not a place to reimplement backend authorization.
- `apps/maple-research/frontend/src-tauri/src` owns privileged device behavior: filesystem and
  process access, native networking, local listeners, deep links, PDF/OCR,
  credential-bearing native clients, and OS integration.
- `apps/maple-research/frontend/src-tauri/src/agent.rs` and `agent/` own the transport-neutral Agent
  runtime, provider adapter, developer tools, permissions, trusted project
  skills, and system-prompt policy. Keep public TypeScript contracts
  Maple-owned; do not leak Goose, RMCP, or ACP types through the Tauri API.
- OpenSecret owns authentication and authorization truth, encrypted
  persistence, provider credentials and routing, model canonicalization,
  protected-route enforcement, and inference-usage capture. Maple may present
  billing and flags API state, but it is not authorization or accounting truth.

Prefer the narrowest existing layer. If a change crosses React, Tauri, an SDK,
and OpenSecret, write down the contract at each boundary before editing.

For every added or changed privileged Tauri command:

1. Put the local privileged effect in `apps/maple-research/frontend/src-tauri/src` behind a narrow
   Rust command. Treat its renderer arguments as untrusted and enforce account,
   canonical path, allowed-root, file-type, size, and lifecycle constraints at
   the native authority boundary as applicable.
2. Register the command in every intended platform's `generate_handler!` list,
   preserve the surrounding `cfg` gates, and verify it is not exposed on an
   unintended target.
3. Put the typed renderer bridge in `apps/maple-research/frontend/src/services` and use an explicit
   platform guard. Components should consume that bridge instead of growing a
   second native contract.
4. Inspect `apps/maple-research/frontend/src-tauri/Cargo.toml`, plugin initialization,
   `apps/maple-research/frontend/src-tauri/tauri.conf.json`, CSP, and
   `apps/maple-research/frontend/src-tauri/capabilities/*.json` separately. Plugin permissions and
   application-defined Rust commands are different authority paths. With the repository's default
   Tauri build configuration, `generate_handler!` registration is the exposure
   boundary for a custom command used by local WebViews; plugin capability
   entries do not narrow that access. Filesystem plugin scopes constrain
   filesystem plugin calls; they do not constrain `std::fs`, Tokio filesystem,
   or other local effects performed by a custom Rust command.
   Enforce a custom command's scope in Rust, and do not broaden a plugin
   permission unless the renderer directly needs that plugin API.
5. Add focused tests for native validation and the typed caller. Where no
   checked-in React-to-IPC integration test exists, manually exercise the exact
   desktop app from UI entry point through IPC to the native effect, including
   a representative rejection case.

## Security and privacy invariants

- Treat the WebView and all Tauri command arguments as untrusted. Revalidate
  account identity, paths, URLs, sizes, ports, enum values, and lifecycle
  ownership in Rust before a privileged effect.
- Never log access or refresh tokens, API keys, raw deep-link URLs, prompts,
  response contents, MCP headers/environments, or decrypted backend payloads.
  Sanitize native errors before emitting them to the renderer.
- Credential-bearing backend URLs must be HTTPS, except explicit loopback HTTP
  in development. Reject embedded credentials, unexpected paths, query
  strings, and fragments.
- Preserve account isolation. Every user-sensitive runtime, cache, file,
  pending operation, event, and query key needs an account owner or opaque
  account scope. After every `await`, late work must prove it still owns the
  current account/session/run before publishing state.
- Account transition and secure logout must stop and drain Agent/ACP, stop and
  scrub the local proxy, clear billing session tokens, clear native auth, and
  dispose account-owned UI state in the established order. Do not add a direct
  sign-out shortcut that bypasses cleanup.
- Project roots grant context and trust; they are not filesystem containment.
  Read-only Agent mode is a consent policy, not an OS sandbox.
- Permission grants are one-use capabilities bound to the exact account,
  session, run, calling surface, request, and payload. Unknown, duplicate,
  late, cancelled, or cross-surface responses fail closed.
- Shell/web permission classifiers fail closed. Cancellation, revocation,
  timeout, and output overflow must terminate spawned process groups and
  revoke credential-bearing tool contexts before another launch.
- STDIO MCP is intentional arbitrary local-code execution. Saving a definition
  is inert; enabling it launches the executable. Never pass its command through
  a shell. Treat persisted MCP configuration as sensitive; verify storage,
  fallback, and deletion guarantees from the implementation.
- Keep local-proxy defaults loopback-only, CORS-off, and auto-start-off. Browser
  reachability and saved-key fallback must remain mutually exclusive.
- Model output and remote Markdown are untrusted. Preserve sanitization,
  external-link confirmation, CSP, Tauri capability, and opener restrictions.
- `VITE_*` is public build-time configuration. Provider credentials and
  administrative billing/flags credentials never belong in Maple.

For auth, proxy, Agent tooling, native capabilities, filesystem or process
access, deep links, or persistence, load `$review-maple-security` before
changing code. Keep resulting findings in the task's review output or another
explicitly authorized destination. Keep only durable security standards and
review methodology in this guide and its skills.

## Runtime and concurrency conventions

- Use the versions and platform dependencies pinned by `flake.nix`; do not add
  an alternate toolchain bootstrap to project docs or CI.
- Use Bun from `apps/maple-research/frontend/`; use Cargo from `apps/maple-research/frontend/src-tauri/`.
- Prefer repository desktop recipes because they provision the pinned ONNX
  Runtime. `just desktop-dev` applies an active local Tauri config overlay;
  standard desktop build recipes retain the standard application identity.
  Use `just desktop-build-debug-overlay` when an unsigned, overlay-configured
  package is required.
- Use `just clean-local`. Raw `cargo clean` may erase a shared Nix Cargo build
  directory used by other checkouts.
- Follow existing React, TypeScript, Rust, error, test, and accessibility
  patterns in the nearest code. Do not perform drive-by migrations.
- Preserve explicit cancellation and ownership tokens in concurrent code.
  Navigation, the currently selected chat, and component lifetime are not
  sufficient ownership proofs for background streams.
- Clean SSE iterator EOF without the protocol's terminal event is truncation,
  not success. Retry cleanup removes only state created by that attempt.
- A Goose dependency bump requires re-diffing Maple's system prompt and
  reviewing the intentional prompt-drift test.

Do not edit generated files by hand, including
`apps/maple-research/frontend/src/routeTree.gen.ts`. Regenerate platform projects or lockfiles with
their repository workflow, inspect all generated deltas, and never hide them
with `git update-index --assume-unchanged`.
