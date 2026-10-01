# Maple Research agent guide

This guide applies to the existing React/Vite/Tauri client under
`apps/maple-research/`, including desktop Agent Mode. Read the
[root guide](../../AGENTS.md); all command/source paths below use the monorepo root.
The directory name does not change shipped application identity.

## Start here

1. Choose the task's [environment and login path](../../docs/development-environments.md).
   Preserve externally managed `.env.local`, `.local/tauri-workspace.json`,
   ports, and processes. Standalone setup applies only to unowned resources.
2. Follow [Research setup](README.md#quick-start), using pinned Nix, `just install`,
   and `./setup-hooks.sh`. Create `.env.local` only when absent; never overwrite
   generated or existing configuration.
3. Load `$develop-maple` for ordinary implementation, `$change-maple-agent-mode`
   for embedded Goose/ACP/MCP work, `$develop-opensecret-sdk` or
   `$develop-maple-proxy` for those components, and `$validate-maple` for checks.
   Use `$review-maple-security` for a trust-boundary change or security review.
4. Select the runtime that exercises the change: `just dev` is browser-only;
   `just desktop-dev` includes native/Agent Mode behavior. Research iOS recipes
   are Tauri commands, not instructions for an unrelated native Swift client.
   These recipes consume existing configuration; for hosted Dev browser work,
   use the [explicit Dev profile loop](../../docs/development-environments.md#research-hosted-dev-browser-loop).

## Placement and essential contracts

- Research chat uses the TypeScript SDK and Responses/Conversations APIs.
  Desktop Agent Mode uses Maple-owned Tauri bridges, embedded Goose, and the
  Rust SDK. ACP is an external edge into that same runtime; neither path
  internally routes through the user-facing local proxy.
- UI/routes/account-scoped state belong in `frontend/src/`; privileged device
  effects and runtime policy belong in `frontend/src-tauri/src/`. OpenSecret
  owns backend authentication, authorization, persistence, providers, and usage.
- Treat renderer/IPC, deep-link, tool, model, and file input as untrusted.
  Revalidate account, path, URL, size, type, permission, and lifecycle at the
  native boundary. Tauri command registration and plugin capabilities are
  separate exposure paths; read [native contracts](docs/development-contracts.md#code-ownership-and-placement)
  before adding or changing a privileged command.
- Preserve account ownership after every async boundary. Logout/account changes
  must drain Agent/ACP, stop/scrub the proxy, clear billing/native auth, and
  dispose account-owned state through established cleanup. Permission grants
  are one-use and bound to the exact account/session/run/request/payload.
- Project trust and read-only Agent mode are not OS containment. STDIO MCP is
  local-code execution; saving is inert, enabling launches. Keep process
  groups, credentials, timeout/cancellation, and revocation under Maple ownership.
- Preserve CSP, sanitization, external-link confirmation, loopback/CORS-off
  proxy defaults, and fail-closed native validation. Secrets never belong in
  `VITE_*`, renderer logs, native errors, or credential-bearing URLs.
- Clean stream EOF without its terminal event is truncation. Retry cleanup
  removes only that attempt's state. Preserve account/run ownership for late
  completions. Read [implementation contracts](docs/development-contracts.md)
  only for the relevant runtime, security, and concurrency sections.
- Consumers independently select SDK versions. Follow the
  [version policy](../../docs/sdk-publishing.md#consumer-version-policy) and
  inspect manifests/locks; TypeScript/Rust transports and coverage differ.
  Use generators for routes/platform projects and inspect generated deltas.

## Validation is proportional evidence

Use `$validate-maple` to select component checks and the exact runtime scenario.
For a privileged IPC change without a checked-in integration harness, manually
exercise the real UI, command, native validation/effect, and a rejection case
through the exact app. A browser test does not cover Tauri or Agent Mode.

The pre-commit hook runs Research frontend format/lint/typecheck/tests and
Rust formatting/tests when those files are staged. It does not run Research
Clippy, integration, flake checks, or packaging. `just rust-lint` is a separate
strict diagnostic; current Research CI is not a strict Clippy gate.

PR packaging scripts replace local endpoints with a fixed hosted Dev profile.
A configured local-backend smoke uses the preserved local environment and
`just desktop-dev` (or the appropriate local mobile runtime). Verify effective
API/auth origins, client ID, account, exact executable/bundle ID, and listeners.
Debug/simulator/package success alone does not establish Local or runtime proof.
See [platform and smoke references](../../.agents/skills/validate-maple/SKILL.md).

## Publication authority

Follow the root authority boundary. Routine work does not authorize master
pushes, signed builds, TestFlight/store uploads, releases, or live changes.
Use `$release-maple` only for requested release work and the
[Pages guide](../../docs/pages-deployments.md) for its independent profiles
and publisher controls. Keep revision-specific findings in the task, and update
material workflow/contract changes in the owning guide/reference.
