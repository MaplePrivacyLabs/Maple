---
name: validate-maple
description: Select and run Maple component checks, platform builds, and exact-runtime smoke evidence for the changed behavior. Use for requested validation or when implementation crosses API/account, Tauri IPC, Agent, auth, persistence, mobile, or packaging boundaries.
---

# Validate Maple

Read root and owning component `AGENTS.md`, the diff/source/tests, and the
workflow/script that owns each check. Classify by changed behavior, not just
path: frontend code may cross native IPC even when CI selects web-only.

## Choose the environment and evidence

Follow [development environments](../../../docs/development-environments.md).
Any backend change, heavy feature, or specific frontend/backend/log/billing
interaction uses an isolated linked Local stack. Small frontend work may
intentionally use hosted Dev with a valid Dev account/login. Keep effective
API/project, auth route, account, and dependent services in the selected lane.
A debug build, simulator, or package does not prove Local.

Use the union of checks for changed boundaries; add relevant lower-level
checks rather than blindly running unrelated platforms or live services.

| Changed boundary | Evidence before handoff |
| --- | --- |
| Prose/inert metadata | Verify consumed result, paths/links/examples/claims; flake checks for workflow/Nix/release configuration |
| Isolated frontend | Focused then complete frontend checks, PR web build, browser smoke of changed states/accessibility |
| API/account/persistence/stream | Applicable frontend gates plus configured client/backend smoke: relevant login/logout/account switch, failure, cancel, reload/history |
| Native IPC/Agent/proxy/device effects | Applicable Rust gates, affected platform build, exact-app user action through native result and representative rejection |
| Mobile/lifecycle/deep links/payments | Affected simulator/device runtime in addition to compilation; exercise changed permission/background/return-link paths |
| Release/signing/distribution | Required platform/artifact evidence; publication only through `$release-maple` with explicit authority |

For privileged Tauri commands, verify every target's registration/`cfg`, typed
bridge, caller, native validation, plugins and capabilities. A registered
custom command and a plugin permission are separate exposure paths. Without
a checked-in React-to-IPC harness, manual exact-app smoke is required. Read
[Research native contracts](../../../apps/maple-research/docs/development-contracts.md#code-ownership-and-placement).

## Load the relevant checks

- [Automated/platform checks](references/automated-checks.md): component CI,
  fixed PR builds, and affected platform recipes. Read only those sections.
- [Runtime smoke](references/runtime-smoke.md): configured local desktop, exact
  app identity, Agent/MCP/proxy, deep links, PDF/OCR, and optional macOS picker
  automation details. Read only the scenarios needed.
- `$develop-opensecret-sdk`, `$develop-maple-proxy`, and `$develop-maple-agent`:
  component gates and selected SDK/dependency-source proof.
- `$validate-opensecret`: backend, disposable DB, and in-tree SDK compatibility.
- `$review-maple-security`: trust-boundary change or security review.

Use pinned Nix environments and preserve generated configuration/process
ownership. CI scripts may reinstall dependencies and hide dotenv; inspect
effects before using them concurrently with a dev runtime. Hooks are fast
component-selected checks, not integration/packaging proof. Research Rust
CI/hook runs tests (hook also formatting), without Clippy; strict
`just rust-lint` is an optional separate diagnostic.

PR builds deliberately replace local endpoints with fixed hosted Dev profiles.
For configured Local smoke use the owning development runtime, supported
local password/account fixture, and verified effective configuration. Native
OAuth requires an intentional hosted/provider scenario; local API selection
alone does not configure its browser origin. Do not fabricate tokens or weaken
attestation when account setup or networking blocks runtime proof.

## Report what was proved

Record commit/dirty state, exact commands/platform/results, build profile,
effective API/auth/project and dependent endpoints, runtime path/bundle ID and
listener ownership, account/data scope, observed scenarios, and unverified
layers. Separate automated, artifact, runtime, integration, and blocked evidence.
An unsigned package is not a release; a browser is not native proof; a unit or
build pass is not end-to-end evidence. Cleanup targets only resources created
by this task through their owning lifecycle.
